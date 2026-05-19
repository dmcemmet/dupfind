# dupfinder

A terminal-based duplicate file finder with a TUI interface. Scans directories using a fast 3-pass algorithm, supports resumable scans, and provides an interactive interface for reviewing and removing duplicates.

## Features

- **TUI interface** — tree-based file browser with file preview (text and images)
- **3-pass scanning** — size grouping → partial hash → full hash for minimal I/O
- **Resumable scans** — interrupted scans can be resumed from the last checkpoint
- **Network-friendly** — partial hashing reads only first/last 4KB, minimizing network traffic
- **Trash or delete** — move duplicates to a local trash folder or permanently delete
- **File preview** — toggleable inline text preview (first 4KB) and image thumbnails filling the preview pane
- **Exclude patterns** — glob-based directory exclusion (e.g. `@Recycle`, `.git`)
- **Read-only mode** — browse duplicates without any deletion capability
- **Sorting** — sort groups and tree by name, size, date, or count
- **Filtering** — live filter to narrow down results by path
- **Result caching** — completed scan results are cached for instant reload (ESC to skip validation on slow drives)
- **File sizes** — displayed inline with middle-truncated filenames to fit the pane

## Installation

```sh
cargo build --release
```

The binary will be at `target/release/dupfinder`.

## Usage

```
dupfinder <path> [OPTIONS]
```

| Argument | Description |
|----------|-------------|
| `<path>` | Directory to scan for duplicates |
| `--ro` | Read-only mode: disables all file deletion |
| `-e, --exclude <pattern>` | Exclude directories matching glob patterns (repeatable) |

### Examples

```sh
# Scan a directory
dupfinder ~/Documents

# Scan a NAS mount in read-only mode, excluding recycle bins
dupfinder /mnt/nas --ro --exclude "@*" --exclude ".git"
```

## Keyboard Shortcuts

| Key | Action |
|-----|--------|
| `j` / `↓` | Move down |
| `k` / `↑` | Move up |
| `Home` | Jump to top of list |
| `End` | Jump to bottom of list |
| `PgUp` / `PgDn` | Page up / page down |
| `Enter` | Expand/collapse directory |
| `Tab` | Switch focus between tree and duplicates pane |
| `Space` | Toggle file selection (in both panes) |
| `a` | Select all files in current directory (left pane) / all copies in group (right pane) |
| `x` | Keep highlighted file, select all other copies for deletion |
| `i` | Invert selection in current directory (left) / group (right) |
| `u` | Deselect all |
| `d` | Open delete dialog (disabled in read-only mode) |
| `s` | Cycle sort mode (name → size → date → count) |
| `c` | Collapse all directories |
| `e` | Expand all directories |
| `p` | Toggle preview pane |
| `g` | Go to selected file's directory in tree (from duplicates pane) |
| `o` | Open selected file with system default application |
| `E` | Export duplicate report to CSV |
| `/` | Start filtering by path |
| `Esc` / `q` | Quit (confirms if files are selected) |

In the delete dialog:

| Key | Action |
|-----|--------|
| `←` / `→` / `Tab` | Switch between Cancel / Trash / Delete buttons |
| `Enter` | Confirm selected action |
| `Esc` | Cancel |

## How Scanning Works

dupfinder uses a 3-pass architecture to minimize disk I/O:

1. **Walk** — Recursively traverses the directory tree collecting file paths and sizes. Files with unique sizes are immediately eliminated (no two files of different sizes can be duplicates).

2. **Partial Hash** — For files sharing the same size, reads only the first and last 4KB and computes an xxHash-64 digest. Files with unique partial hashes are eliminated. This avoids reading entire large files over slow connections.

3. **Full Hash** — Only files that collide on both size and partial hash are fully read and hashed. For files ≤ 8KB, the partial hash already covers the entire content, so no additional I/O is needed. Full hashing runs in parallel using rayon.

State is checkpointed to disk every 60 seconds during each phase, enabling resume after interruption.

## State Files

dupfinder stores state in `~/.dupfinder/`:

| File | Purpose |
|------|---------|
| `<hash>.scan` | Incremental scan state (walk progress, partial hash results). Enables resume. |
| `<hash>.bin` | Cached final results (duplicate groups). Enables instant reload without rescanning. |

The `<hash>` is an xxHash-64 of the canonical root path. Files are serialized with bincode.

On launch, if state files exist, dupfinder prompts to use cached results, resume an incomplete scan, or start fresh.

## License

MIT
