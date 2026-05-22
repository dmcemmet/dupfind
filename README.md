# dupfinder

A terminal-based duplicate file finder with a TUI interface. Scans directories using a multi-pass algorithm optimized for network drives, supports resumable scans, and provides an interactive interface for reviewing and removing duplicates.

## Features

- **Multi-directory scanning** — scan one or more directories, find duplicates within and across them
- **4-stage scanning** — size grouping → head hash → tail hash → full verification
- **Network-optimized** — two-stage partial hashing minimizes seeks; prefetch queue keeps I/O saturated
- **Fast mode** — `--fast N` samples N middle chunks instead of full-reading files (configurable accuracy)
- **Parallel everything** — directory walking, hashing, and verification all use rayon
- **Prefetch queue** — background thread pre-reads files into OS page cache ahead of hash threads
- **TUI interface** — tree-based file browser with file preview (text and images)
- **Async preview** — previews load in background thread, UI stays responsive
- **Resumable scans** — interrupted scans can be resumed from the last checkpoint
- **Trash or delete** — move duplicates to a local trash folder or permanently delete
- **File preview** — toggleable inline text/image preview filling the preview pane
- **Mouse support** — scroll wheel navigates, click switches pane focus
- **Exclude patterns** — glob-based directory exclusion (e.g. `@Recycle`, `.git`)
- **Min-size filter** — skip files below a threshold
- **Read-only mode** — browse duplicates without any deletion capability
- **Dry-run mode** — print duplicate report without launching TUI
- **Sorting** — sort groups and tree by name, size, date, or count
- **Filtering** — live filter to narrow down results by path
- **Result caching** — completed scan results cached for instant reload (ESC to skip validation)
- **File sizes** — displayed inline with middle-truncated filenames
- **Flash notifications** — action feedback (export, open, delete)
- **Export to CSV** — save duplicate report for external processing

## Installation

```sh
cargo build --release
```

The binary will be at `target/release/dupfinder`.

## Usage

```
dupfinder <path>... [OPTIONS]
```

| Option | Description |
|--------|-------------|
| `<path>...` | One or more directories to scan for duplicates |
| `--ro` | Read-only mode: disables all file deletion |
| `-e, --exclude <pattern>` | Exclude directories matching glob patterns (repeatable) |
| `--min-size <size>` | Minimum file size (e.g. `1MB`, `500KB`, `1024`) |
| `--fast [N]` | Fast mode: sample N middle chunks instead of full hash (default: 1) |
| `--dry-run` | Print duplicate report without launching TUI |

### Examples

```sh
# Scan a single directory
dupfinder ~/Documents

# Scan multiple directories (finds cross-directory duplicates)
dupfinder ~/Photos /mnt/nas/photos ~/Backup/photos

# NAS-optimized: fast mode with 3 samples, skip small files
dupfinder /mnt/nas --fast 3 --min-size 1MB --exclude "@*"

# Read-only mode, excluding recycle bins
dupfinder /mnt/nas --ro --exclude "@*" --exclude ".git"

# Dry-run to see what would be found
dupfinder ~/Documents --dry-run
```

## Keyboard Shortcuts

| Key | Action |
|-----|--------|
| `j` / `↓` | Move down |
| `k` / `↑` | Move up |
| `Home` / `End` | Jump to top / bottom |
| `PgUp` / `PgDn` | Page up / page down |
| `Enter` | Expand/collapse directory |
| `Tab` | Switch focus between tree and duplicates pane |
| `Space` | Toggle file selection (in both panes) |
| `a` | Select all files in current directory (left) / all copies in group (right) |
| `x` | Keep highlighted file, select all other copies for deletion |
| `i` | Invert selection in current directory (left) / group (right) |
| `u` | Deselect all |
| `d` | Open delete dialog (disabled in read-only mode) |
| `s` | Cycle sort mode (name → size → date → count) |
| `c` / `e` | Collapse / expand all directories |
| `p` | Toggle preview pane |
| `g` | Go to selected file's directory in tree (from duplicates pane) |
| `o` | Open selected file with system default application |
| `E` | Export duplicate report to CSV |
| `S` | Sync all — select all and start delete |
| `/` | Start filtering by path |
| `Esc` / `q` | Quit (confirms if files are selected) |

In the delete dialog:

| Key | Action |
|-----|--------|
| `←` / `→` / `Tab` | Switch between Cancel / Trash / Delete buttons |
| `Enter` | Confirm selected action |
| `Esc` | Cancel |

## How Scanning Works

dupfinder uses a 4-stage pipeline optimized for network drives:

```
Stage 0: Walk        — collect all files + sizes (parallel directory reading)
Stage 1: Head hash   — hash first 4KB only (eliminates ~80% of candidates)
Stage 2: Tail hash   — hash last 4KB for head-collisions (eliminates most remaining)
Stage 3: Verify      — full hash (or --fast middle sampling) for final confirmation
```

### Network Optimizations

- **Parallel directory walking** — multiple directories read simultaneously via rayon
- **Two-stage partial hash** — head-only first (1 seek), tail only for collisions (2nd seek)
- **Prefetch queue** — background thread pre-reads files into OS page cache
- **Parallel hashing** — rayon keeps multiple I/O requests in-flight
- **`--fast N` mode** — samples N×4KB from evenly-distributed positions instead of reading entire file

### Fast Mode (`--fast N`)

Instead of reading the entire file for verification, samples `N` evenly-distributed 4KB chunks from the middle region (between head and tail). Total I/O per file: `(N+2) × 4KB`.

For a 1GB file with `--fast 5`: reads 28KB instead of 1GB (99.997% less I/O).

The number of samples is automatically capped to what fits in the file's middle region. For a 20KB file with `--fast 10`: middle = 12KB = 3 possible chunks, so it caps to 3.

**False positive risk**: effectively zero for real files (photos, videos, documents). Would require two files to be identical at all sample points but differ only in the gaps — doesn't happen with real media.

## State Files

dupfinder stores state in `~/.dupfinder/`:

| File | Purpose |
|------|---------|
| `<hash>.scan` | Incremental scan state (walk progress, partial hash results). Enables resume. |
| `<hash>.bin` | Cached final results (duplicate groups). Enables instant reload without rescanning. |

The `<hash>` is an xxHash-64 of the canonical root path. Files are serialized with bincode.

On launch, if state files exist, dupfinder prompts to use cached results, resume an incomplete scan, or start fresh.

## Testing

Run the integration test script:

```sh
./test.sh
```

This creates a temporary directory with various test scenarios (small/large files, cross-directory duplicates, tricky edge cases), runs dupfinder in all modes (normal, --fast, single-root, multi-root, --exclude, --min-size), and verifies results with assertions. The temp directory is automatically cleaned up.

Unit tests:

```sh
cargo test
```

## License

MIT
