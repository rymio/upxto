use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Read, Stdout, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::execute;
use crossterm::queue;
use crossterm::style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor};
use crossterm::terminal::{
    self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
    enable_raw_mode,
};
use rusqlite::{Connection, OptionalExtension, params};

const DEFAULT_DB_FILE: &str = ".upxto/upxto.db";
const DEFAULT_PROJECT: &str = "default";
const HASH_OFFSET: u64 = 0xcbf29ce484222325;
const HASH_PRIME: u64 = 0x100000001b3;

type DynResult<T> = Result<T, Box<dyn std::error::Error>>;

#[derive(Debug, Default)]
struct Config {
    index_production: Option<PathBuf>,
    index_fresh: Option<PathBuf>,
    show_changes: bool,
    backup_production: bool,
    update_production: bool,
    tui: bool,
    dry_run: bool,
    delete_missing: bool,
    state_file: PathBuf,
    backup_dir: Option<PathBuf>,
    help: bool,
}

#[derive(Debug, Default)]
struct State {
    project_name: Option<String>,
    left_dir: Option<PathBuf>,
    right_dir: Option<PathBuf>,
    production_root: Option<PathBuf>,
    fresh_root: Option<PathBuf>,
    production: BTreeMap<String, FileEntry>,
    fresh: BTreeMap<String, FileEntry>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct FileEntry {
    size: u64,
    hash: u64,
}

#[derive(Debug)]
enum ChangeKind {
    New,
    Updated,
    Deleted,
}

#[derive(Debug)]
struct Change {
    path: String,
    kind: ChangeKind,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum FileStatus {
    Same,
    New,
    Updated,
    Deleted,
}

#[derive(Debug)]
struct TuiRow {
    fresh_path: Option<String>,
    production_path: Option<String>,
    status: FileStatus,
}

#[derive(Debug)]
struct DiffRow {
    fresh: Option<String>,
    fresh_line: Option<usize>,
    production: Option<String>,
    production_line: Option<usize>,
    changed: bool,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum ActivePanel {
    Left,
    Right,
}

#[derive(Debug)]
struct BrowserPanel {
    current_dir: PathBuf,
    entries: Vec<FsEntry>,
    selected: usize,
}

#[derive(Debug)]
struct FsEntry {
    name: String,
    path: PathBuf,
    is_dir: bool,
}

struct Spinner {
    frame: usize,
}

impl Spinner {
    fn new() -> Self {
        Self { frame: 0 }
    }

    fn next(&mut self) -> char {
        const FRAMES: [char; 4] = ['|', '/', '-', '\\'];
        let frame = FRAMES[self.frame % FRAMES.len()];
        self.frame += 1;
        frame
    }
}

fn main() {
    if let Err(err) = run() {
        eprintln!("upxto: {err}");
        std::process::exit(1);
    }
}

fn run() -> DynResult<()> {
    let config = parse_args(env::args().skip(1))?;

    if config.help || no_actions(&config) {
        print_help();
        return Ok(());
    }

    let mut state = load_state(&config.state_file)?;

    if let Some(path) = &config.index_production {
        let root = canonical_dir(path)?;
        let files = index_folder(&root)?;
        println!(
            "Indexed production: {} files from {}",
            files.len(),
            root.display()
        );
        state.production_root = Some(root);
        state.production = files;
        save_state(&state, &config.state_file)?;
    }

    if let Some(path) = &config.index_fresh {
        let root = canonical_dir(path)?;
        let files = index_folder(&root)?;
        println!(
            "Indexed fresh: {} files from {}",
            files.len(),
            root.display()
        );
        state.fresh_root = Some(root);
        state.fresh = files;
        save_state(&state, &config.state_file)?;
    }

    if config.show_changes {
        ensure_indexes(&state)?;
        let changes = compare_indexes(&state, config.delete_missing);
        print_changes(&changes);
    }

    if config.tui {
        run_tui(
            &mut state,
            config.delete_missing,
            config.state_file.clone(),
            config.backup_dir.as_deref(),
        )?;
    }

    if config.backup_production {
        let production_root = state
            .production_root
            .as_deref()
            .ok_or("production root is not indexed yet; run --index-production <folder> first")?;
        let backup_root = config.backup_dir.clone().unwrap_or_else(default_backup_dir);
        let backup_path = backup_root.join(format!("production-{}", timestamp()));
        copy_dir(production_root, &backup_path)?;
        println!("Backed up production to {}", backup_path.display());
    }

    if config.update_production {
        ensure_indexes(&state)?;
        let production_root = state.production_root.as_deref().unwrap();
        let fresh_root = state.fresh_root.as_deref().unwrap();
        let changes = compare_indexes(&state, config.delete_missing);
        apply_changes(
            &changes,
            production_root,
            fresh_root,
            config.delete_missing,
            config.dry_run,
        )?;

        if !config.dry_run {
            state.production = index_folder(production_root)?;
            save_state(&state, &config.state_file)?;
            println!("Production index refreshed after update.");
        }
    }

    Ok(())
}

fn parse_args<I>(args: I) -> DynResult<Config>
where
    I: IntoIterator<Item = String>,
{
    let mut config = Config {
        state_file: PathBuf::from(DEFAULT_DB_FILE),
        ..Config::default()
    };

    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--index-production" => {
                config.index_production = Some(next_path(&mut iter, "--index-production")?);
            }
            "--index-fresh" => {
                config.index_fresh = Some(next_path(&mut iter, "--index-fresh")?);
            }
            "--show-changes" => config.show_changes = true,
            "--tui" => config.tui = true,
            "--backup-production" => config.backup_production = true,
            "--update-production" | "--apply" => config.update_production = true,
            "--dry-run" => config.dry_run = true,
            "--delete-missing" => config.delete_missing = true,
            "--state" => config.state_file = next_path(&mut iter, "--state")?,
            "--backup-dir" => config.backup_dir = Some(next_path(&mut iter, "--backup-dir")?),
            "-h" | "--help" => config.help = true,
            other => return Err(format!("unknown argument: {other}").into()),
        }
    }

    Ok(config)
}

fn next_path<I>(iter: &mut I, option: &str) -> DynResult<PathBuf>
where
    I: Iterator<Item = String>,
{
    iter.next()
        .map(PathBuf::from)
        .ok_or_else(|| format!("{option} requires a folder or file path").into())
}

fn no_actions(config: &Config) -> bool {
    config.index_production.is_none()
        && config.index_fresh.is_none()
        && !config.show_changes
        && !config.tui
        && !config.backup_production
        && !config.update_production
}

fn print_help() {
    println!(
        "\
UPXTO - compare and update a production folder from a fresh folder

Usage:
  upxto --tui
  upxto --index-production <folder>
  upxto --index-fresh <folder>
  upxto --show-changes
  upxto --index-production <folder> --index-fresh <folder> --tui
  upxto --backup-production
  upxto --update-production

Common workflow:
  upxto --tui

Inside TUI:
  Type or load a project name
  Press L at project prompt to list saved projects
  Navigate left panel to fresh folder, press Ctrl+A
  Navigate right panel to production folder, press Ctrl+S
  Use F3 diff, F5 copy selected new file, F9 mkdir, F10 update

Options:
  --tui                      Open project-aware two-panel folder browser
  --apply                    Alias for --update-production
  --dry-run                  Show what would be copied/deleted
  --delete-missing           Treat files absent from fresh as deletions
  --state <file>             SQLite database path, default .upxto/upxto.db
  --backup-dir <folder>      Backup destination, default .upxto/backups
  -h, --help                 Show help

Default excludes:
  .git, .upxto, __pycache__, .pytest_cache, .mypy_cache, node_modules,
  target, venv, .venv
"
    );
}

fn canonical_dir(path: &Path) -> DynResult<PathBuf> {
    let canonical = fs::canonicalize(path)?;
    if !canonical.is_dir() {
        return Err(format!("not a directory: {}", canonical.display()).into());
    }
    Ok(canonical)
}

fn index_folder(root: &Path) -> DynResult<BTreeMap<String, FileEntry>> {
    let mut files = BTreeMap::new();
    visit_files(root, root, &mut |path| {
        let rel = relative_path(root, path)?;
        let entry = file_entry(path)?;
        files.insert(rel, entry);
        Ok(())
    })?;
    Ok(files)
}

fn visit_files<F>(root: &Path, current: &Path, on_file: &mut F) -> DynResult<()>
where
    F: FnMut(&Path) -> DynResult<()>,
{
    let mut entries = fs::read_dir(current)?.collect::<Result<Vec<_>, io::Error>>()?;
    entries.sort_by_key(|entry| entry.path());

    for entry in entries {
        let path = entry.path();
        let file_type = entry.file_type()?;

        if file_type.is_dir() {
            if should_skip_dir(entry.file_name().as_os_str()) {
                continue;
            }
            visit_files(root, &path, on_file)?;
        } else if file_type.is_file() {
            on_file(&path)?;
        }
    }

    let _ = root;
    Ok(())
}

fn should_skip_dir(name: &OsStr) -> bool {
    matches!(
        name.to_str(),
        Some(
            ".git"
                | ".upxto"
                | "__pycache__"
                | ".pytest_cache"
                | ".mypy_cache"
                | "node_modules"
                | "target"
                | "venv"
                | ".venv"
        )
    )
}

fn relative_path(root: &Path, path: &Path) -> DynResult<String> {
    let rel = path.strip_prefix(root)?;
    Ok(rel
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/"))
}

fn file_entry(path: &Path) -> DynResult<FileEntry> {
    let metadata = fs::metadata(path)?;
    Ok(FileEntry {
        size: metadata.len(),
        hash: hash_file(path)?,
    })
}

fn hash_file(path: &Path) -> DynResult<u64> {
    let mut file = File::open(path)?;
    let mut buffer = [0_u8; 64 * 1024];
    let mut hash = HASH_OFFSET;

    loop {
        let bytes = file.read(&mut buffer)?;
        if bytes == 0 {
            break;
        }
        for byte in &buffer[..bytes] {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(HASH_PRIME);
        }
    }

    Ok(hash)
}

fn compare_indexes(state: &State, include_deleted: bool) -> Vec<Change> {
    let mut changes = Vec::new();

    for (path, fresh_entry) in &state.fresh {
        match state.production.get(path) {
            None => changes.push(Change {
                path: path.clone(),
                kind: ChangeKind::New,
            }),
            Some(prod_entry) if prod_entry != fresh_entry => changes.push(Change {
                path: path.clone(),
                kind: ChangeKind::Updated,
            }),
            Some(_) => {}
        }
    }

    if include_deleted {
        let fresh_paths: BTreeSet<&String> = state.fresh.keys().collect();
        for path in state.production.keys() {
            if !fresh_paths.contains(path) {
                changes.push(Change {
                    path: path.clone(),
                    kind: ChangeKind::Deleted,
                });
            }
        }
    }

    changes.sort_by(|left, right| left.path.cmp(&right.path));
    changes
}

fn print_changes(changes: &[Change]) {
    if changes.is_empty() {
        println!("No new or updated files found.");
        return;
    }

    let mut new_count = 0;
    let mut updated_count = 0;
    let mut deleted_count = 0;

    for change in changes {
        match change.kind {
            ChangeKind::New => {
                new_count += 1;
                println!("NEW     {}", change.path);
            }
            ChangeKind::Updated => {
                updated_count += 1;
                println!("UPDATE  {}", change.path);
            }
            ChangeKind::Deleted => {
                deleted_count += 1;
                println!("DELETE  {}", change.path);
            }
        }
    }

    println!("\nSummary: {new_count} new, {updated_count} updated, {deleted_count} deleted");
}

fn run_tui(
    state: &mut State,
    include_deleted: bool,
    state_file: PathBuf,
    backup_dir: Option<&Path>,
) -> DynResult<()> {
    let mut terminal = TerminalSession::start()?;
    if let Some(loaded_state) = prompt_project(&mut terminal.stdout, &state_file, state)? {
        *state = loaded_state;
    }

    let start_dir = env::current_dir()?;
    let mut left = BrowserPanel::new(
        state
            .left_dir
            .clone()
            .or_else(|| state.fresh_root.clone())
            .unwrap_or_else(|| start_dir.clone()),
    )?;
    let mut right = BrowserPanel::new(
        state
            .right_dir
            .clone()
            .or_else(|| state.production_root.clone())
            .unwrap_or(start_dir),
    )?;
    let mut active_panel = ActivePanel::Left;
    let mut message = String::from("Ready");
    let mut spinner = Spinner::new();

    loop {
        draw_tui(
            &mut terminal.stdout,
            state,
            &left,
            &right,
            active_panel,
            &message,
        )?;

        if let Event::Key(key) = event::read()? {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => {
                    state.left_dir = Some(left.current_dir.clone());
                    state.right_dir = Some(right.current_dir.clone());
                    save_state(state, &state_file)?;
                    break;
                }
                KeyCode::F(1) => run_help_view(&mut terminal.stdout)?,
                KeyCode::Tab => active_panel = active_panel.other(),
                KeyCode::Up => active_browser_mut(&mut left, &mut right, active_panel).move_up(),
                KeyCode::Down => {
                    active_browser_mut(&mut left, &mut right, active_panel).move_down()
                }
                KeyCode::PageUp => {
                    active_browser_mut(&mut left, &mut right, active_panel).page_up()
                }
                KeyCode::PageDown => {
                    active_browser_mut(&mut left, &mut right, active_panel).page_down()
                }
                KeyCode::Home => active_browser_mut(&mut left, &mut right, active_panel).home(),
                KeyCode::End => active_browser_mut(&mut left, &mut right, active_panel).end(),
                KeyCode::Enter => {
                    active_browser_mut(&mut left, &mut right, active_panel).enter_selected()?;
                    message = current_panel_message(active_panel, &left, &right);
                }
                KeyCode::Backspace => {
                    active_browser_mut(&mut left, &mut right, active_panel).go_parent()?;
                    message = current_panel_message(active_panel, &left, &right);
                }
                KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    draw_spinner(
                        &mut terminal.stdout,
                        &mut spinner,
                        "Indexing fresh folder...",
                    )?;
                    message = match index_left_as_fresh(state, &left, &state_file) {
                        Ok(result) => result,
                        Err(err) => format!("Ctrl+A failed: {err}"),
                    };
                }
                KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    draw_spinner(
                        &mut terminal.stdout,
                        &mut spinner,
                        "Indexing production folder...",
                    )?;
                    message = match index_right_as_production(state, &right, &state_file) {
                        Ok(result) => result,
                        Err(err) => format!("Ctrl+S failed: {err}"),
                    };
                }
                KeyCode::F(3) => {
                    if let Some(row) = selected_browser_row(state, &left) {
                        if row.status != FileStatus::Same {
                            run_diff_view(&mut terminal.stdout, state, &row)?;
                        } else {
                            message = "F3: selected file is already the same".to_string();
                        }
                    } else {
                        message =
                            "F3: select a left-panel file after both roots are indexed".to_string();
                    }
                }
                KeyCode::F(5) => {
                    message = match selected_browser_row(state, &left) {
                        Some(row) => {
                            draw_spinner(
                                &mut terminal.stdout,
                                &mut spinner,
                                "Copying file and reindexing...",
                            )?;
                            match copy_selected_new_file(state, Some(&row), &state_file) {
                                Ok(result) => {
                                    left.refresh()?;
                                    right.refresh()?;
                                    result
                                }
                                Err(err) => format!("F5 failed: {err}"),
                            }
                        }
                        None => "F5: select a left-panel NEW file after both roots are indexed"
                            .to_string(),
                    };
                }
                KeyCode::F(7) => run_about_view(&mut terminal.stdout)?,
                KeyCode::F(8) => {
                    draw_spinner(
                        &mut terminal.stdout,
                        &mut spinner,
                        "Backing up production...",
                    )?;
                    message = match backup_production_from_tui(state, backup_dir) {
                        Ok(path) => match reindex_existing_roots(state, &state_file) {
                            Ok(summary) => {
                                format!("F8 backup created: {}; {summary}", path.display())
                            }
                            Err(err) => format!(
                                "F8 backup created: {}; reindex failed: {err}",
                                path.display()
                            ),
                        },
                        Err(err) => format!("F8 backup failed: {err}"),
                    };
                }
                KeyCode::F(9) => {
                    message = match mkdir_in_active_panel(
                        &mut terminal.stdout,
                        state,
                        &mut left,
                        &mut right,
                        active_panel,
                        &state_file,
                    ) {
                        Ok(result) => result,
                        Err(err) => format!("F9 mkdir failed: {err}"),
                    };
                }
                KeyCode::F(10) => {
                    draw_spinner(
                        &mut terminal.stdout,
                        &mut spinner,
                        "Deploying new and changed files...",
                    )?;
                    message = match deploy_updates_from_tui(state, &state_file) {
                        Ok(result) => {
                            left.refresh()?;
                            right.refresh()?;
                            result
                        }
                        Err(err) => format!("F10 update failed: {err}"),
                    };
                }
                _ => {}
            }
        }

        state.left_dir = Some(left.current_dir.clone());
        state.right_dir = Some(right.current_dir.clone());
        let _ = include_deleted;
    }

    Ok(())
}

impl ActivePanel {
    fn other(self) -> Self {
        match self {
            ActivePanel::Left => ActivePanel::Right,
            ActivePanel::Right => ActivePanel::Left,
        }
    }
}

impl BrowserPanel {
    fn new(path: PathBuf) -> DynResult<Self> {
        let mut panel = Self {
            current_dir: canonical_dir(&path).or_else(|_| env::current_dir())?,
            entries: Vec::new(),
            selected: 0,
        };
        panel.refresh()?;
        Ok(panel)
    }

    fn refresh(&mut self) -> DynResult<()> {
        self.entries = read_browser_entries(&self.current_dir)?;
        self.selected = self.selected.min(self.entries.len().saturating_sub(1));
        Ok(())
    }

    fn selected_entry(&self) -> Option<&FsEntry> {
        self.entries.get(self.selected)
    }

    fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    fn move_down(&mut self) {
        if self.selected + 1 < self.entries.len() {
            self.selected += 1;
        }
    }

    fn page_up(&mut self) {
        self.selected = self.selected.saturating_sub(10);
    }

    fn page_down(&mut self) {
        self.selected = self
            .selected
            .saturating_add(10)
            .min(self.entries.len().saturating_sub(1));
    }

    fn home(&mut self) {
        self.selected = 0;
    }

    fn end(&mut self) {
        self.selected = self.entries.len().saturating_sub(1);
    }

    fn enter_selected(&mut self) -> DynResult<()> {
        let Some(entry) = self.selected_entry() else {
            return Ok(());
        };
        if entry.is_dir {
            self.current_dir = canonical_dir(&entry.path)?;
            self.selected = 0;
            self.refresh()?;
        }
        Ok(())
    }

    fn go_parent(&mut self) -> DynResult<()> {
        if let Some(parent) = self.current_dir.parent() {
            self.current_dir = parent.to_path_buf();
            self.selected = 0;
            self.refresh()?;
        }
        Ok(())
    }
}

fn active_browser_mut<'a>(
    left: &'a mut BrowserPanel,
    right: &'a mut BrowserPanel,
    active_panel: ActivePanel,
) -> &'a mut BrowserPanel {
    match active_panel {
        ActivePanel::Left => left,
        ActivePanel::Right => right,
    }
}

fn read_browser_entries(path: &Path) -> DynResult<Vec<FsEntry>> {
    let mut dirs = Vec::new();
    let mut files = Vec::new();

    if let Some(parent) = path.parent() {
        dirs.push(FsEntry {
            name: "[..]".to_string(),
            path: parent.to_path_buf(),
            is_dir: true,
        });
    }

    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let name = entry.file_name().to_string_lossy().to_string();
        if file_type.is_dir() {
            dirs.push(FsEntry {
                name: format!("[{name}]"),
                path: entry.path(),
                is_dir: true,
            });
        } else if file_type.is_file() {
            files.push(FsEntry {
                name,
                path: entry.path(),
                is_dir: false,
            });
        }
    }

    dirs.sort_by_key(|entry| entry.name.to_lowercase());
    files.sort_by_key(|entry| entry.name.to_lowercase());
    dirs.extend(files);
    Ok(dirs)
}

fn current_panel_message(
    active_panel: ActivePanel,
    left: &BrowserPanel,
    right: &BrowserPanel,
) -> String {
    match active_panel {
        ActivePanel::Left => format!("Left: {}", left.current_dir.display()),
        ActivePanel::Right => format!("Right: {}", right.current_dir.display()),
    }
}

fn index_left_as_fresh(
    state: &mut State,
    left: &BrowserPanel,
    state_file: &Path,
) -> DynResult<String> {
    let root = canonical_dir(&left.current_dir)?;
    let files = index_folder(&root)?;
    let count = files.len();
    state.left_dir = Some(root.clone());
    state.fresh_root = Some(root.clone());
    state.fresh = files;
    save_state(state, state_file)?;
    Ok(format!(
        "Ctrl+A indexed fresh: {count} files from {}",
        root.display()
    ))
}

fn index_right_as_production(
    state: &mut State,
    right: &BrowserPanel,
    state_file: &Path,
) -> DynResult<String> {
    let root = canonical_dir(&right.current_dir)?;
    let files = index_folder(&root)?;
    let count = files.len();
    state.right_dir = Some(root.clone());
    state.production_root = Some(root.clone());
    state.production = files;
    save_state(state, state_file)?;
    Ok(format!(
        "Ctrl+S indexed production: {count} files from {}",
        root.display()
    ))
}

fn selected_browser_row(state: &State, left: &BrowserPanel) -> Option<TuiRow> {
    let entry = left.selected_entry()?;
    if entry.is_dir {
        return None;
    }
    let fresh_root = state.fresh_root.as_ref()?;
    let rel = entry.path.strip_prefix(fresh_root).ok()?;
    let path = rel
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    let fresh_entry = state.fresh.get(&path);
    let production_entry = state.production.get(&path);
    let status = match (fresh_entry, production_entry) {
        (Some(_), None) => FileStatus::New,
        (Some(fresh), Some(production)) if fresh == production => FileStatus::Same,
        (Some(_), Some(_)) => FileStatus::Updated,
        (None, Some(_)) => FileStatus::Deleted,
        (None, None) => return None,
    };

    Some(TuiRow {
        fresh_path: fresh_entry.map(|_| path.clone()),
        production_path: production_entry.map(|_| path),
        status,
    })
}

fn prompt_project(
    stdout: &mut Stdout,
    db_path: &Path,
    current_state: &State,
) -> DynResult<Option<State>> {
    let mut input = current_state.project_name.clone().unwrap_or_default();

    loop {
        draw_project_prompt(stdout, &input)?;

        if let Event::Key(key) = event::read()? {
            match key.code {
                KeyCode::Char('l') | KeyCode::Char('L')
                    if !key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    if let Some(project_name) = select_project_from_list(stdout, db_path)? {
                        if let Some(mut state) = load_project_state(db_path, &project_name)? {
                            state.project_name = Some(project_name);
                            return Ok(Some(state));
                        }
                    }
                }
                KeyCode::Enter => {
                    let name = input.trim();
                    if name.is_empty() {
                        return Ok(None);
                    }
                    let project_name = sanitize_project_name(name);
                    let mut state = match load_project_state(db_path, &project_name)? {
                        Some(state) => state,
                        None => {
                            let mut state = current_state.clone_for_project();
                            state.project_name = Some(project_name.clone());
                            save_state(&state, db_path)?;
                            state
                        }
                    };
                    state.project_name = Some(project_name);
                    return Ok(Some(state));
                }
                KeyCode::Esc => return Ok(None),
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    input.push(ch);
                }
                _ => {}
            }
        }
    }
}

fn draw_project_prompt(stdout: &mut Stdout, input: &str) -> DynResult<()> {
    let (width, height) = terminal::size()?;
    let lines = [
        "UPXTO Project",
        "",
        "Type project name and press Enter to load or create it.",
        "Press L to list saved projects.",
        "Leave blank and press Enter for the default session.",
        "Esc also starts the default session.",
        "",
    ];
    let start_y = height.saturating_sub(8).saturating_div(2);

    queue!(stdout, Clear(ClearType::All), MoveTo(0, 0))?;
    for (index, line) in lines.iter().enumerate() {
        let y = start_y + index as u16;
        if y >= height {
            break;
        }
        queue!(stdout, MoveTo(0, y))?;
        if index == 0 {
            queue!(
                stdout,
                SetForegroundColor(Color::White),
                SetAttribute(Attribute::Bold),
                Print(fit_text(line, width as usize)),
                ResetColor,
                SetAttribute(Attribute::Reset)
            )?;
        } else {
            queue!(
                stdout,
                SetForegroundColor(Color::Grey),
                Print(fit_text(line, width as usize)),
                ResetColor
            )?;
        }
    }

    let prompt_y = start_y + lines.len() as u16;
    queue!(
        stdout,
        MoveTo(0, prompt_y),
        SetForegroundColor(Color::Cyan),
        Print(fit_text(&format!("Project: {input}"), width as usize)),
        ResetColor
    )?;
    stdout.flush()?;
    Ok(())
}

fn select_project_from_list(stdout: &mut Stdout, db_path: &Path) -> DynResult<Option<String>> {
    let projects = list_project_names(db_path)?;
    let mut selected = 0_usize;

    loop {
        draw_project_list(stdout, &projects, selected)?;

        if let Event::Key(key) = event::read()? {
            match key.code {
                KeyCode::Enter => return Ok(projects.get(selected).cloned()),
                KeyCode::Char('q') | KeyCode::Esc => return Ok(None),
                KeyCode::Up => selected = selected.saturating_sub(1),
                KeyCode::Down => {
                    if selected + 1 < projects.len() {
                        selected += 1;
                    }
                }
                KeyCode::PageUp => selected = selected.saturating_sub(10),
                KeyCode::PageDown => {
                    selected = selected
                        .saturating_add(10)
                        .min(projects.len().saturating_sub(1));
                }
                KeyCode::Home => selected = 0,
                KeyCode::End => selected = projects.len().saturating_sub(1),
                _ => {}
            }
        }
    }
}

fn draw_project_list(stdout: &mut Stdout, projects: &[String], selected: usize) -> DynResult<()> {
    let (width, height) = terminal::size()?;
    let visible_rows = height.saturating_sub(5) as usize;
    let top = scroll_top(selected, visible_rows, projects.len());

    queue!(stdout, Clear(ClearType::All), MoveTo(0, 0))?;
    queue!(
        stdout,
        MoveTo(0, 0),
        SetForegroundColor(Color::White),
        SetAttribute(Attribute::Bold),
        Print(fit_text("Load Project", width as usize)),
        ResetColor,
        SetAttribute(Attribute::Reset),
        MoveTo(0, 1),
        SetForegroundColor(Color::DarkGrey),
        Print(fit_text(
            "Enter loads | Arrows move | q/Esc returns",
            width as usize
        )),
        ResetColor
    )?;

    if projects.is_empty() {
        queue!(
            stdout,
            MoveTo(0, 3),
            SetForegroundColor(Color::Grey),
            Print(fit_text("No saved projects yet.", width as usize)),
            ResetColor
        )?;
    } else {
        for screen_row in 0..visible_rows {
            let index = top + screen_row;
            let y = 3 + screen_row as u16;
            if y >= height {
                break;
            }
            clear_line(stdout, y, width)?;
            if let Some(project) = projects.get(index) {
                queue!(stdout, MoveTo(0, y), SetForegroundColor(Color::Cyan))?;
                if index == selected {
                    queue!(stdout, SetAttribute(Attribute::Reverse))?;
                }
                queue!(
                    stdout,
                    Print(fit_text(project, width as usize)),
                    ResetColor,
                    SetAttribute(Attribute::Reset)
                )?;
            }
        }
    }

    stdout.flush()?;
    Ok(())
}

impl State {
    fn clone_for_project(&self) -> Self {
        Self {
            project_name: self.project_name.clone(),
            left_dir: self.left_dir.clone(),
            right_dir: self.right_dir.clone(),
            production_root: self.production_root.clone(),
            fresh_root: self.fresh_root.clone(),
            production: self.production.clone(),
            fresh: self.fresh.clone(),
        }
    }
}

fn sanitize_project_name(project_name: &str) -> String {
    let name = project_name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();

    if name.is_empty() {
        "default".to_string()
    } else {
        name
    }
}

fn copy_selected_new_file(
    state: &mut State,
    row: Option<&TuiRow>,
    state_file: &Path,
) -> DynResult<String> {
    let Some(row) = row else {
        return Ok("F5: no file selected".to_string());
    };

    if row.status != FileStatus::New {
        return Ok(
            "F5: only NEW files are copied; updated files are inspect-only for now".to_string(),
        );
    }

    let rel_path = row
        .fresh_path
        .as_deref()
        .ok_or("selected row does not exist in fresh folder")?;
    let fresh_root = state.fresh_root.as_deref().ok_or("fresh root is missing")?;
    let production_root = state
        .production_root
        .as_deref()
        .ok_or("production root is missing")?;
    let source = fresh_root.join(rel_path);
    let target = production_root.join(rel_path);

    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(&source, &target)?;

    let summary = reindex_existing_roots(state, state_file)?;

    Ok(format!("F5 copied new file: {rel_path}; {summary}"))
}

fn backup_production_from_tui(state: &State, backup_dir: Option<&Path>) -> DynResult<PathBuf> {
    let production_root = state
        .production_root
        .as_deref()
        .ok_or("production root is not indexed yet")?;
    let backup_root = backup_dir
        .map(PathBuf::from)
        .unwrap_or_else(default_backup_dir);
    let backup_path = backup_root.join(format!("production-{}", timestamp()));
    copy_dir(production_root, &backup_path)?;
    Ok(backup_path)
}

fn mkdir_in_active_panel(
    stdout: &mut Stdout,
    state: &mut State,
    left: &mut BrowserPanel,
    right: &mut BrowserPanel,
    active_panel: ActivePanel,
    state_file: &Path,
) -> DynResult<String> {
    let Some(name) = prompt_text(stdout, "F9 mkdir", "Directory name")? else {
        return Ok("F9 mkdir cancelled".to_string());
    };
    let name = name.trim();
    if name.is_empty() {
        return Ok("F9 mkdir cancelled".to_string());
    }
    if name.contains('/') || name.contains('\\') {
        return Ok("F9 mkdir: use a single folder name, not a path".to_string());
    }

    let panel = active_browser_mut(left, right, active_panel);
    let mut spinner = Spinner::new();
    draw_spinner(stdout, &mut spinner, "Creating folder and reindexing...")?;
    let new_dir = panel.current_dir.join(name);
    fs::create_dir_all(&new_dir)?;
    panel.refresh()?;
    select_entry_by_path(panel, &new_dir);

    let summary = reindex_existing_roots(state, state_file)?;
    Ok(format!("F9 created {}; {summary}", new_dir.display()))
}

fn deploy_updates_from_tui(state: &mut State, state_file: &Path) -> DynResult<String> {
    ensure_indexes(state)?;
    let production_root = state.production_root.as_deref().unwrap();
    let fresh_root = state.fresh_root.as_deref().unwrap();
    let changes = compare_indexes(state, false);
    let deploy_count = changes
        .iter()
        .filter(|change| matches!(change.kind, ChangeKind::New | ChangeKind::Updated))
        .count();

    copy_changes_quiet(&changes, production_root, fresh_root)?;
    let summary = reindex_existing_roots(state, state_file)?;

    Ok(format!(
        "F10 deployed {deploy_count} new/updated files; {summary}"
    ))
}

fn copy_changes_quiet(
    changes: &[Change],
    production_root: &Path,
    fresh_root: &Path,
) -> DynResult<()> {
    for change in changes {
        if !matches!(change.kind, ChangeKind::New | ChangeKind::Updated) {
            continue;
        }

        let source = fresh_root.join(&change.path);
        let target = production_root.join(&change.path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(source, target)?;
    }

    Ok(())
}

fn reindex_existing_roots(state: &mut State, state_file: &Path) -> DynResult<String> {
    let mut parts = Vec::new();

    if let Some(root) = state.fresh_root.clone() {
        state.fresh = index_folder(&root)?;
        parts.push(format!("fresh {} files", state.fresh.len()));
    }
    if let Some(root) = state.production_root.clone() {
        state.production = index_folder(&root)?;
        parts.push(format!("production {} files", state.production.len()));
    }

    save_state(state, state_file)?;

    if parts.is_empty() {
        Ok("no indexed roots yet".to_string())
    } else {
        Ok(format!("reindexed {}", parts.join(", ")))
    }
}

fn select_entry_by_path(panel: &mut BrowserPanel, path: &Path) {
    if let Some(index) = panel.entries.iter().position(|entry| entry.path == path) {
        panel.selected = index;
    }
}

fn prompt_text(stdout: &mut Stdout, title: &str, label: &str) -> DynResult<Option<String>> {
    let mut input = String::new();

    loop {
        draw_text_prompt(stdout, title, label, &input)?;

        if let Event::Key(key) = event::read()? {
            match key.code {
                KeyCode::Enter => return Ok(Some(input)),
                KeyCode::Esc => return Ok(None),
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    input.push(ch);
                }
                _ => {}
            }
        }
    }
}

fn draw_text_prompt(stdout: &mut Stdout, title: &str, label: &str, input: &str) -> DynResult<()> {
    let (width, height) = terminal::size()?;
    let start_y = height.saturating_sub(5).saturating_div(2);

    queue!(stdout, Clear(ClearType::All), MoveTo(0, 0))?;
    queue!(
        stdout,
        MoveTo(0, start_y),
        SetForegroundColor(Color::White),
        SetAttribute(Attribute::Bold),
        Print(fit_text(title, width as usize)),
        ResetColor,
        SetAttribute(Attribute::Reset),
        MoveTo(0, start_y + 2),
        SetForegroundColor(Color::Cyan),
        Print(fit_text(&format!("{label}: {input}"), width as usize)),
        ResetColor,
        MoveTo(0, start_y + 4),
        SetForegroundColor(Color::DarkGrey),
        Print(fit_text("Enter confirms | Esc cancels", width as usize)),
        ResetColor
    )?;
    stdout.flush()?;
    Ok(())
}

fn draw_spinner(stdout: &mut Stdout, spinner: &mut Spinner, message: &str) -> DynResult<()> {
    let (width, height) = terminal::size()?;
    let text = format!("{} {}", spinner.next(), message);
    let x = width
        .saturating_sub(text.chars().count() as u16)
        .saturating_div(2);
    let y = height.saturating_div(2);

    queue!(
        stdout,
        Clear(ClearType::All),
        MoveTo(x, y),
        SetForegroundColor(Color::Cyan),
        SetAttribute(Attribute::Bold),
        Print(text),
        ResetColor,
        SetAttribute(Attribute::Reset),
        MoveTo(0, height.saturating_sub(1)),
        SetForegroundColor(Color::DarkGrey),
        Print(fit_text("Please wait...", width as usize)),
        ResetColor
    )?;
    stdout.flush()?;
    Ok(())
}

fn draw_tui(
    stdout: &mut Stdout,
    state: &State,
    left: &BrowserPanel,
    right: &BrowserPanel,
    active_panel: ActivePanel,
    message: &str,
) -> DynResult<()> {
    let (width, height) = terminal::size()?;
    let left_width = width / 2;
    let right_width = width.saturating_sub(left_width);
    let visible_rows = height.saturating_sub(5) as usize;
    let left_top = scroll_top(left.selected, visible_rows, left.entries.len());
    let right_top = scroll_top(right.selected, visible_rows, right.entries.len());

    queue!(stdout, Clear(ClearType::All), MoveTo(0, 0))?;
    draw_header(stdout, state, width)?;
    draw_panel_header(
        stdout,
        0,
        2,
        left_width,
        &format!("Fresh {}", left.current_dir.display()),
    )?;
    draw_panel_header(
        stdout,
        left_width,
        2,
        right_width,
        &format!("Production {}", right.current_dir.display()),
    )?;

    for screen_row in 0..visible_rows {
        let y = 3 + screen_row as u16;
        clear_line(stdout, y, width)?;

        if let Some(entry) = left.entries.get(left_top + screen_row) {
            draw_browser_cell(
                stdout,
                0,
                y,
                left_width,
                entry,
                entry_status_for_panel(state, ActivePanel::Left, entry),
                active_panel == ActivePanel::Left && left_top + screen_row == left.selected,
            )?;
        }
        if let Some(entry) = right.entries.get(right_top + screen_row) {
            draw_browser_cell(
                stdout,
                left_width,
                y,
                right_width,
                entry,
                entry_status_for_panel(state, ActivePanel::Right, entry),
                active_panel == ActivePanel::Right && right_top + screen_row == right.selected,
            )?;
        }
    }

    let footer_y = height.saturating_sub(1);
    clear_line(stdout, footer_y, width)?;
    queue!(
        stdout,
        MoveTo(0, footer_y),
        SetForegroundColor(Color::DarkGrey),
        Print(fit_text(
            &format!(
                "F1 help | Tab panel | Enter open | Backspace/.. up | Ctrl+A fresh | Ctrl+S prod | F3 diff | F5 new | F8 backup | F9 mkdir | F10 update | {}",
                message
            ),
            width as usize
        )),
        ResetColor
    )?;
    stdout.flush()?;
    Ok(())
}

fn draw_browser_cell(
    stdout: &mut Stdout,
    x: u16,
    y: u16,
    width: u16,
    entry: &FsEntry,
    status: Option<FileStatus>,
    selected: bool,
) -> DynResult<()> {
    if width == 0 {
        return Ok(());
    }

    let marker = if entry.is_dir {
        "/"
    } else {
        match status {
            Some(FileStatus::New) => "+",
            Some(FileStatus::Updated) => "*",
            Some(FileStatus::Deleted) => "-",
            Some(FileStatus::Same) => " ",
            None => " ",
        }
    };
    let color = if entry.is_dir {
        Color::Cyan
    } else {
        status.map(status_color).unwrap_or(Color::Grey)
    };
    let text = format!("{marker} {}", entry.name);

    queue!(stdout, MoveTo(x, y), SetForegroundColor(color))?;
    if selected {
        queue!(stdout, SetAttribute(Attribute::Reverse))?;
    }
    queue!(
        stdout,
        Print(fit_text(&text, width.saturating_sub(1) as usize)),
        ResetColor,
        SetAttribute(Attribute::Reset)
    )?;
    Ok(())
}

fn entry_status_for_panel(
    state: &State,
    panel: ActivePanel,
    entry: &FsEntry,
) -> Option<FileStatus> {
    if entry.is_dir {
        return None;
    }

    let (this_root, this_index, other_index) = match panel {
        ActivePanel::Left => (state.fresh_root.as_ref()?, &state.fresh, &state.production),
        ActivePanel::Right => (
            state.production_root.as_ref()?,
            &state.production,
            &state.fresh,
        ),
    };
    let rel = entry.path.strip_prefix(this_root).ok()?;
    let path = rel
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    let this_entry = this_index.get(&path)?;
    match other_index.get(&path) {
        None => Some(FileStatus::New),
        Some(other_entry) if other_entry == this_entry => Some(FileStatus::Same),
        Some(_) => Some(FileStatus::Updated),
    }
}

fn run_diff_view(stdout: &mut Stdout, state: &State, row: &TuiRow) -> DynResult<()> {
    let diff_rows = build_diff_rows(state, row)?;
    let mut selected = 0_usize;

    loop {
        draw_diff_view(stdout, state, row, &diff_rows, selected)?;

        if let Event::Key(key) = event::read()? {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc | KeyCode::F(3) => break,
                KeyCode::F(1) => run_help_view(stdout)?,
                KeyCode::F(7) => run_about_view(stdout)?,
                KeyCode::Up => selected = selected.saturating_sub(1),
                KeyCode::Down => {
                    if selected + 1 < diff_rows.len() {
                        selected += 1;
                    }
                }
                KeyCode::PageUp => selected = selected.saturating_sub(10),
                KeyCode::PageDown => {
                    selected = selected
                        .saturating_add(10)
                        .min(diff_rows.len().saturating_sub(1));
                }
                KeyCode::Home => selected = 0,
                KeyCode::End => selected = diff_rows.len().saturating_sub(1),
                _ => {}
            }
        }
    }

    Ok(())
}

fn build_diff_rows(state: &State, row: &TuiRow) -> DynResult<Vec<DiffRow>> {
    let fresh_lines = match &row.fresh_path {
        Some(path) => read_file_lines(state.fresh_root.as_deref().unwrap().join(path))?,
        None => Vec::new(),
    };
    let production_lines = match &row.production_path {
        Some(path) => read_file_lines(state.production_root.as_deref().unwrap().join(path))?,
        None => Vec::new(),
    };

    Ok(align_lines(&fresh_lines, &production_lines))
}

fn read_file_lines(path: PathBuf) -> DynResult<Vec<String>> {
    let bytes = fs::read(path)?;
    let text = String::from_utf8_lossy(&bytes);
    Ok(text.lines().map(|line| line.to_string()).collect())
}

fn align_lines(fresh: &[String], production: &[String]) -> Vec<DiffRow> {
    let mut lcs = vec![vec![0_usize; production.len() + 1]; fresh.len() + 1];

    for fresh_index in (0..fresh.len()).rev() {
        for production_index in (0..production.len()).rev() {
            lcs[fresh_index][production_index] = if fresh[fresh_index]
                == production[production_index]
            {
                lcs[fresh_index + 1][production_index + 1] + 1
            } else {
                lcs[fresh_index + 1][production_index].max(lcs[fresh_index][production_index + 1])
            };
        }
    }

    let mut rows = Vec::new();
    let mut fresh_index = 0;
    let mut production_index = 0;

    while fresh_index < fresh.len() || production_index < production.len() {
        if fresh_index < fresh.len()
            && production_index < production.len()
            && fresh[fresh_index] == production[production_index]
        {
            rows.push(DiffRow {
                fresh: Some(fresh[fresh_index].clone()),
                fresh_line: Some(fresh_index + 1),
                production: Some(production[production_index].clone()),
                production_line: Some(production_index + 1),
                changed: false,
            });
            fresh_index += 1;
            production_index += 1;
        } else if fresh_index < fresh.len()
            && production_index < production.len()
            && lcs[fresh_index + 1][production_index] == lcs[fresh_index][production_index + 1]
        {
            rows.push(DiffRow {
                fresh: Some(fresh[fresh_index].clone()),
                fresh_line: Some(fresh_index + 1),
                production: Some(production[production_index].clone()),
                production_line: Some(production_index + 1),
                changed: true,
            });
            fresh_index += 1;
            production_index += 1;
        } else if production_index >= production.len()
            || (fresh_index < fresh.len()
                && lcs[fresh_index + 1][production_index] > lcs[fresh_index][production_index + 1])
        {
            rows.push(DiffRow {
                fresh: Some(fresh[fresh_index].clone()),
                fresh_line: Some(fresh_index + 1),
                production: None,
                production_line: None,
                changed: true,
            });
            fresh_index += 1;
        } else {
            rows.push(DiffRow {
                fresh: None,
                fresh_line: None,
                production: Some(production[production_index].clone()),
                production_line: Some(production_index + 1),
                changed: true,
            });
            production_index += 1;
        }
    }

    if rows.is_empty() {
        rows.push(DiffRow {
            fresh: None,
            fresh_line: None,
            production: None,
            production_line: None,
            changed: false,
        });
    }

    rows
}

fn draw_diff_view(
    stdout: &mut Stdout,
    state: &State,
    row: &TuiRow,
    diff_rows: &[DiffRow],
    selected: usize,
) -> DynResult<()> {
    let (width, height) = terminal::size()?;
    let left_width = width / 2;
    let right_width = width.saturating_sub(left_width);
    let visible_rows = height.saturating_sub(5) as usize;
    let top = scroll_top(selected, visible_rows, diff_rows.len());
    let path = row
        .fresh_path
        .as_deref()
        .or(row.production_path.as_deref())
        .unwrap_or("(unknown)");

    queue!(stdout, Clear(ClearType::All), MoveTo(0, 0))?;
    draw_diff_header(stdout, state, path, width)?;
    draw_panel_header(stdout, 0, 2, left_width, "Fresh")?;
    draw_panel_header(stdout, left_width, 2, right_width, "Production")?;

    for screen_row in 0..visible_rows {
        let row_index = top + screen_row;
        let y = 3 + screen_row as u16;
        clear_line(stdout, y, width)?;

        if let Some(diff_row) = diff_rows.get(row_index) {
            let is_selected = row_index == selected;
            draw_diff_cell(
                stdout,
                0,
                y,
                left_width,
                diff_row.fresh_line,
                diff_row.fresh.as_deref().unwrap_or(""),
                diff_row.changed,
                is_selected,
            )?;
            draw_diff_cell(
                stdout,
                left_width,
                y,
                right_width,
                diff_row.production_line,
                diff_row.production.as_deref().unwrap_or(""),
                diff_row.changed,
                is_selected,
            )?;
        }
    }

    let footer_y = height.saturating_sub(1);
    clear_line(stdout, footer_y, width)?;
    queue!(
        stdout,
        MoveTo(0, footer_y),
        SetForegroundColor(Color::DarkGrey),
        Print(format!(
            "Diff view | F1 help | F7 about | Arrows scroll | PgUp/PgDn | Home/End | F3/q/Esc back | {} lines",
            diff_rows.len()
        )),
        ResetColor
    )?;
    stdout.flush()?;
    Ok(())
}

fn draw_diff_header(stdout: &mut Stdout, state: &State, path: &str, width: u16) -> DynResult<()> {
    let fresh = state
        .fresh_root
        .as_ref()
        .map(|root| root.join(path).display().to_string())
        .unwrap_or_else(|| "-".to_string());
    let production = state
        .production_root
        .as_ref()
        .map(|root| root.join(path).display().to_string())
        .unwrap_or_else(|| "-".to_string());
    let title = fit_text(
        &format!("UPXTO F3 diff  fresh: {fresh}  |  production: {production}"),
        width as usize,
    );

    queue!(
        stdout,
        MoveTo(0, 0),
        SetForegroundColor(Color::White),
        SetAttribute(Attribute::Bold),
        Print(title),
        ResetColor,
        SetAttribute(Attribute::Reset)
    )?;
    Ok(())
}

fn draw_diff_cell(
    stdout: &mut Stdout,
    x: u16,
    y: u16,
    width: u16,
    line_number: Option<usize>,
    text: &str,
    changed: bool,
    selected: bool,
) -> DynResult<()> {
    if width == 0 {
        return Ok(());
    }

    let color = if changed { Color::Red } else { Color::Green };
    let marker = if changed { "!" } else { " " };
    let number = line_number
        .map(|number| number.to_string())
        .unwrap_or_else(|| "-".to_string());
    let line = if text.is_empty() {
        format!("{marker}{number:>5} |")
    } else {
        format!("{marker}{number:>5} | {text}")
    };

    queue!(stdout, MoveTo(x, y), SetForegroundColor(color))?;
    if selected {
        queue!(stdout, SetAttribute(Attribute::Reverse))?;
    }
    queue!(
        stdout,
        Print(fit_text(&line, width.saturating_sub(1) as usize)),
        ResetColor,
        SetAttribute(Attribute::Reset)
    )?;
    Ok(())
}

fn run_help_view(stdout: &mut Stdout) -> DynResult<()> {
    loop {
        draw_help_view(stdout)?;

        if let Event::Key(key) = event::read()? {
            match key.code {
                KeyCode::F(1) | KeyCode::Char('q') | KeyCode::Esc => break,
                _ => {}
            }
        }
    }

    Ok(())
}

fn draw_help_view(stdout: &mut Stdout) -> DynResult<()> {
    let (width, height) = terminal::size()?;
    let lines = [
        "UPXTO TUI Help",
        "",
        "File list",
        "  Tab              Switch active panel",
        "  Up/Down          Move cursor",
        "  PageUp/PageDown  Jump through files",
        "  Home/End         Go to first or last file",
        "  Enter            Open selected folder",
        "  Backspace        Move to parent folder",
        "  [..]             Visible parent folder entry",
        "  Ctrl+A           Index current left folder as fresh",
        "  Ctrl+S           Index current right folder as production",
        "  F3               Show side-by-side diff for selected changed file",
        "  F5               Copy selected NEW file from fresh to production",
        "  F7               Show copyright and license",
        "  F8               Back up the full production folder",
        "  F9               Create folder in active panel",
        "  F10              Deploy all NEW and UPDATE files to production",
        "  q or Esc         Quit",
        "",
        "Diff view",
        "  Up/Down          Scroll lines",
        "  PageUp/PageDown  Jump through lines",
        "  Home/End         Go to first or last line",
        "  F3, q, or Esc    Return to file list",
        "  F7               Show copyright and license",
        "",
        "Colors",
        "  Green            Same",
        "  Red              New, updated, deleted, or changed line",
        "",
        "Press F1, q, or Esc to return.",
    ];

    queue!(stdout, Clear(ClearType::All), MoveTo(0, 0))?;

    for (index, line) in lines.iter().enumerate() {
        let y = index as u16;
        if y >= height {
            break;
        }

        queue!(stdout, MoveTo(0, y))?;
        if index == 0 {
            queue!(
                stdout,
                SetForegroundColor(Color::White),
                SetAttribute(Attribute::Bold),
                Print(fit_text(line, width as usize)),
                ResetColor,
                SetAttribute(Attribute::Reset)
            )?;
        } else if *line == "File list" || *line == "Colors" || *line == "Diff view" {
            queue!(
                stdout,
                SetForegroundColor(Color::Cyan),
                SetAttribute(Attribute::Bold),
                Print(fit_text(line, width as usize)),
                ResetColor,
                SetAttribute(Attribute::Reset)
            )?;
        } else {
            queue!(
                stdout,
                SetForegroundColor(Color::Grey),
                Print(fit_text(line, width as usize)),
                ResetColor
            )?;
        }
    }

    stdout.flush()?;
    Ok(())
}

fn run_about_view(stdout: &mut Stdout) -> DynResult<()> {
    loop {
        draw_about_view(stdout)?;

        if let Event::Key(key) = event::read()? {
            match key.code {
                KeyCode::F(7) | KeyCode::Char('q') | KeyCode::Esc => break,
                _ => {}
            }
        }
    }

    Ok(())
}

fn draw_about_view(stdout: &mut Stdout) -> DynResult<()> {
    let (width, height) = terminal::size()?;
    let lines = [
        "UPXTO",
        "",
        "Copyright Rob Rymarczyk",
        "License MIT",
        "",
        "Press F7, q, or Esc to return.",
    ];
    let start_y = height.saturating_sub(lines.len() as u16).saturating_div(2);

    queue!(stdout, Clear(ClearType::All), MoveTo(0, 0))?;

    for (index, line) in lines.iter().enumerate() {
        let y = start_y.saturating_add(index as u16);
        if y >= height {
            break;
        }

        let line_width = line.chars().count() as u16;
        let x = width.saturating_sub(line_width).saturating_div(2);
        queue!(stdout, MoveTo(x, y))?;

        if index == 0 {
            queue!(
                stdout,
                SetForegroundColor(Color::White),
                SetAttribute(Attribute::Bold),
                Print(*line),
                ResetColor,
                SetAttribute(Attribute::Reset)
            )?;
        } else {
            queue!(
                stdout,
                SetForegroundColor(Color::Grey),
                Print(*line),
                ResetColor
            )?;
        }
    }

    stdout.flush()?;
    Ok(())
}

fn draw_header(stdout: &mut Stdout, state: &State, width: u16) -> DynResult<()> {
    let production = state
        .production_root
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "-".to_string());
    let fresh = state
        .fresh_root
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "-".to_string());
    let title = fit_text(
        &format!("UPXTO  fresh: {fresh}  |  production: {production}"),
        width as usize,
    );

    queue!(
        stdout,
        MoveTo(0, 0),
        SetForegroundColor(Color::White),
        SetAttribute(Attribute::Bold),
        Print(title),
        ResetColor,
        SetAttribute(Attribute::Reset)
    )?;
    Ok(())
}

fn draw_panel_header(
    stdout: &mut Stdout,
    x: u16,
    y: u16,
    width: u16,
    title: &str,
) -> DynResult<()> {
    let label = format!(" {title} ");
    queue!(
        stdout,
        MoveTo(x, y),
        SetForegroundColor(Color::Cyan),
        SetAttribute(Attribute::Bold),
        Print(fit_text(&label, width as usize)),
        ResetColor,
        SetAttribute(Attribute::Reset)
    )?;
    Ok(())
}

fn clear_line(stdout: &mut Stdout, y: u16, width: u16) -> DynResult<()> {
    queue!(
        stdout,
        MoveTo(0, y),
        ResetColor,
        SetAttribute(Attribute::Reset),
        Print(" ".repeat(width as usize))
    )?;
    Ok(())
}

fn status_color(status: FileStatus) -> Color {
    match status {
        FileStatus::Same => Color::Green,
        FileStatus::New | FileStatus::Updated | FileStatus::Deleted => Color::Red,
    }
}

fn scroll_top(selected: usize, visible_rows: usize, total_rows: usize) -> usize {
    if visible_rows == 0 || total_rows <= visible_rows {
        return 0;
    }
    let half_page = visible_rows / 2;
    selected
        .saturating_sub(half_page)
        .min(total_rows.saturating_sub(visible_rows))
}

fn fit_text(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }

    let mut output = text.chars().take(width).collect::<String>();
    let output_len = output.chars().count();
    if output_len < width {
        output.push_str(&" ".repeat(width - output_len));
    }
    output
}

struct TerminalSession {
    stdout: Stdout,
}

impl TerminalSession {
    fn start() -> DynResult<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, Hide)?;
        Ok(Self { stdout })
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = execute!(self.stdout, Show, LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

fn ensure_indexes(state: &State) -> DynResult<()> {
    if state.production_root.is_none() {
        return Err("production is not indexed yet; run --index-production <folder>".into());
    }
    if state.fresh_root.is_none() {
        return Err("fresh folder is not indexed yet; run --index-fresh <folder>".into());
    }
    Ok(())
}

fn apply_changes(
    changes: &[Change],
    production_root: &Path,
    fresh_root: &Path,
    delete_missing: bool,
    dry_run: bool,
) -> DynResult<()> {
    if changes.is_empty() {
        println!("Nothing to update.");
        return Ok(());
    }

    for change in changes {
        let production_path = production_root.join(&change.path);
        let fresh_path = fresh_root.join(&change.path);

        match change.kind {
            ChangeKind::New | ChangeKind::Updated => {
                println!(
                    "{} {}",
                    if dry_run { "WOULD COPY" } else { "COPY      " },
                    change.path
                );
                if !dry_run {
                    if let Some(parent) = production_path.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    fs::copy(&fresh_path, &production_path)?;
                }
            }
            ChangeKind::Deleted if delete_missing => {
                println!(
                    "{} {}",
                    if dry_run { "WOULD DEL " } else { "DELETE    " },
                    change.path
                );
                if !dry_run {
                    fs::remove_file(&production_path)?;
                    remove_empty_parents(production_path.parent(), production_root)?;
                }
            }
            ChangeKind::Deleted => {}
        }
    }

    Ok(())
}

fn remove_empty_parents(mut current: Option<&Path>, stop_at: &Path) -> DynResult<()> {
    while let Some(path) = current {
        if path == stop_at {
            break;
        }
        match fs::remove_dir(path) {
            Ok(()) => current = path.parent(),
            Err(err) if err.kind() == io::ErrorKind::DirectoryNotEmpty => break,
            Err(err) if err.kind() == io::ErrorKind::NotFound => break,
            Err(err) => return Err(err.into()),
        }
    }
    Ok(())
}

fn copy_dir(source: &Path, destination: &Path) -> DynResult<()> {
    fs::create_dir_all(destination)?;
    copy_dir_all(source, source, destination)
}

fn copy_dir_all(root: &Path, current: &Path, destination: &Path) -> DynResult<()> {
    let mut entries = fs::read_dir(current)?.collect::<Result<Vec<_>, io::Error>>()?;
    entries.sort_by_key(|entry| entry.path());

    for entry in entries {
        let path = entry.path();
        let file_type = entry.file_type()?;
        let rel = path.strip_prefix(root)?;
        let target = destination.join(rel);

        if file_type.is_dir() {
            fs::create_dir_all(&target)?;
            copy_dir_all(root, &path, destination)?;
        } else if file_type.is_file() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&path, target)?;
        }
    }

    Ok(())
}

fn default_backup_dir() -> PathBuf {
    PathBuf::from(".upxto/backups")
}

fn timestamp() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    seconds.to_string()
}

fn load_state(path: &Path) -> DynResult<State> {
    Ok(load_project_state(path, DEFAULT_PROJECT)?.unwrap_or_default())
}

fn save_state(state: &State, path: &Path) -> DynResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut connection = Connection::open(path)?;
    init_db(&connection)?;
    let tx = connection.transaction()?;
    let project_name = state.project_name.as_deref().unwrap_or(DEFAULT_PROJECT);

    tx.execute(
        "insert into projects (
            name, left_dir, right_dir, production_root, fresh_root, updated_at
        ) values (?1, ?2, ?3, ?4, ?5, strftime('%s','now'))
        on conflict(name) do update set
            left_dir = excluded.left_dir,
            right_dir = excluded.right_dir,
            production_root = excluded.production_root,
            fresh_root = excluded.fresh_root,
            updated_at = excluded.updated_at",
        params![
            project_name,
            path_to_db_string(state.left_dir.as_ref()),
            path_to_db_string(state.right_dir.as_ref()),
            path_to_db_string(state.production_root.as_ref()),
            path_to_db_string(state.fresh_root.as_ref()),
        ],
    )?;
    tx.execute(
        "delete from files where project_name = ?1",
        params![project_name],
    )?;

    for (rel_path, entry) in &state.production {
        save_file_entry(&tx, project_name, "production", rel_path, entry)?;
    }
    for (rel_path, entry) in &state.fresh {
        save_file_entry(&tx, project_name, "fresh", rel_path, entry)?;
    }

    tx.commit()?;
    Ok(())
}

fn load_project_state(path: &Path, project_name: &str) -> DynResult<Option<State>> {
    if !path.exists() {
        return Ok(None);
    }

    let connection = Connection::open(path)?;
    init_db(&connection)?;
    let project = connection
        .query_row(
            "select left_dir, right_dir, production_root, fresh_root
             from projects where name = ?1",
            params![project_name],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .optional()?;

    let Some((left_dir, right_dir, production_root, fresh_root)) = project else {
        return Ok(None);
    };

    let mut state = State {
        project_name: Some(project_name.to_string()),
        left_dir: left_dir.map(PathBuf::from),
        right_dir: right_dir.map(PathBuf::from),
        production_root: production_root.map(PathBuf::from),
        fresh_root: fresh_root.map(PathBuf::from),
        production: BTreeMap::new(),
        fresh: BTreeMap::new(),
    };

    let mut statement = connection.prepare(
        "select side, rel_path, size, hash from files
         where project_name = ?1 order by side, rel_path",
    )?;
    let rows = statement.query_map(params![project_name], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
        ))
    })?;

    for row in rows {
        let (side, rel_path, size, hash) = row?;
        let entry = FileEntry {
            size: u64::try_from(size)?,
            hash: hash as u64,
        };
        match side.as_str() {
            "production" => {
                state.production.insert(rel_path, entry);
            }
            "fresh" => {
                state.fresh.insert(rel_path, entry);
            }
            _ => {}
        }
    }

    Ok(Some(state))
}

fn list_project_names(path: &Path) -> DynResult<Vec<String>> {
    if !path.exists() {
        return Ok(Vec::new());
    }

    let connection = Connection::open(path)?;
    init_db(&connection)?;
    let mut statement =
        connection.prepare("select name from projects order by updated_at desc, name asc")?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    let mut projects = Vec::new();
    for row in rows {
        projects.push(row?);
    }
    Ok(projects)
}

fn init_db(connection: &Connection) -> DynResult<()> {
    connection.execute_batch(
        "
        create table if not exists projects (
            name text primary key,
            left_dir text,
            right_dir text,
            production_root text,
            fresh_root text,
            updated_at integer not null
        );

        create table if not exists files (
            project_name text not null,
            side text not null check(side in ('production', 'fresh')),
            rel_path text not null,
            size integer not null,
            hash integer not null,
            primary key (project_name, side, rel_path),
            foreign key (project_name) references projects(name) on delete cascade
        );

        create index if not exists idx_files_lookup
            on files(project_name, rel_path, side);
        ",
    )?;
    Ok(())
}

fn save_file_entry(
    connection: &Connection,
    project_name: &str,
    side: &str,
    rel_path: &str,
    entry: &FileEntry,
) -> DynResult<()> {
    connection.execute(
        "insert into files (project_name, side, rel_path, size, hash)
         values (?1, ?2, ?3, ?4, ?5)",
        params![
            project_name,
            side,
            rel_path,
            i64::try_from(entry.size)?,
            entry.hash as i64,
        ],
    )?;
    Ok(())
}

fn path_to_db_string(path: Option<&PathBuf>) -> Option<String> {
    path.map(|path| path.to_string_lossy().to_string())
}
