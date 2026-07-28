# UPXTO

UPXTO is a Rust command-line and TUI tool for comparing a fresh project folder
against a production folder, then safely deploying only the files that are new
or changed.

It is built for workflows where a Django or other application is pulled into a
temporary folder first, reviewed, backed up, and then copied into production.

## Features

- Two-panel Midnight Commander style TUI
- Project-based workflow with saved folder locations
- SQLite-backed file indexes
- Content-hash and size based comparison
- Side-by-side diff view for changed files
- Production backup support
- Copy selected new files or deploy all new/changed files
- Folder navigation, parent folder access, and mkdir from the TUI
- Spinner feedback during longer operations

## Build

```bash
cargo build --release
```

The binary will be created at:

```text
target/release/upxto
```

## Quick Start

Start the TUI:

```bash
upxto --tui
```

At the project prompt:

- Type a project name and press Enter to load or create it.
- Press `L` to list saved projects.
- Press Enter on a blank name, or Esc, to use the default session.

Inside the TUI:

1. Navigate the left panel to your fresh folder.
2. Press `Ctrl+A` to index it as fresh.
3. Navigate the right panel to your production folder.
4. Press `Ctrl+S` to index it as production.
5. Use `F3` to inspect changes.
6. Use `F8` to back up production.
7. Use `F6` to deploy all new/changed files.

## TUI Keys

| Key | Action |
| --- | --- |
| `L` | List saved projects at startup |
| `Tab` | Switch active panel |
| `Up` / `Down` | Move cursor |
| `PageUp` / `PageDown` | Jump through list |
| `Home` / `End` | First or last entry |
| `Enter` | Open selected folder |
| `Backspace` | Move to parent folder |
| `[..]` | Visible parent folder entry |
| `Ctrl+A` | Index current left folder as fresh |
| `Ctrl+S` | Index current right folder as production |
| `F1` | Help |
| `F3` | Side-by-side diff for selected changed file |
| `F5` | Copy selected `NEW` file to production |
| `F6` | Deploy all `NEW` and `UPDATE` files |
| `F7` | About, copyright, and license |
| `F8` | Back up production |
| `F9` | Create folder in active panel |
| `q` / Esc | Quit or return from popup |

## Command-Line Workflow

The non-interactive CLI workflow still works:

```bash
upxto --index-production /srv/myapp
upxto --index-fresh /tmp/myapp-new
upxto --show-changes
upxto --backup-production
upxto --update-production
```

Dry-run before updating:

```bash
upxto --dry-run --update-production
```

Use deletion only when you intentionally want production files removed if they
are missing from the fresh folder:

```bash
upxto --delete-missing --show-changes
upxto --delete-missing --update-production
```

## Commands

```text
--tui                        Open project-aware two-panel folder browser
--index-production <folder>  Index the current production folder
--index-fresh <folder>       Index the fresh/new source folder
--show-changes               Show new and updated files
--backup-production          Copy production to .upxto/backups
--update-production          Copy new and updated files from fresh to production
--apply                      Alias for --update-production
--dry-run                    Show what would happen without changing files
--delete-missing             Also delete production files missing from fresh
--state <file>               Use a custom SQLite database file
--backup-dir <folder>        Use a custom backup folder
-h, --help                   Show help
```

## How Comparison Works

UPXTO indexes files by relative path. For each file it stores:

- relative path
- file size
- 64-bit content hash

Comparison rules:

| Result | Meaning |
| --- | --- |
| `NEW` | File exists in fresh, but not production |
| `UPDATE` | Same relative path exists, but size or hash differs |
| `SAME` | Same relative path, size, and hash |
| `DELETE` | Production file is missing from fresh, only with `--delete-missing` |

## Storage

UPXTO stores projects and indexes in SQLite.

Default database:

```text
.upxto/upxto.db
```

Use another database:

```bash
upxto --state /path/to/upxto.db --tui
```

Tables:

```text
projects  project name, panel folders, fresh root, production root
files     project name, side, relative path, size, content hash
```

## Index Excludes

By default, indexing skips common cache/build folders:

```text
.git
.upxto
__pycache__
.pytest_cache
.mypy_cache
node_modules
target
venv
.venv
```

## Backups

Backups copy the production folder contents recursively.

Default backup location:

```text
.upxto/backups
```

Custom backup location:

```bash
upxto --backup-dir /backups/myapp --backup-production
```

## License

Copyright Rob Rymarczyk.

Licensed under the MIT License.
