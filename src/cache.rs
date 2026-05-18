use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use serde::{Deserialize, Serialize};

use crate::scanner::{DuplicateGroups, FileInfo};

#[derive(Serialize, Deserialize)]
struct CacheEntry {
    root: PathBuf,
    groups: Vec<CachedGroup>,
}

#[derive(Serialize, Deserialize)]
struct CachedGroup {
    size: u64,
    paths: Vec<PathBuf>,
}

fn cache_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".dupfinder"))
}

fn cache_path_for(root: &Path) -> Option<PathBuf> {
    let dir = cache_dir()?;
    let key = format!(
        "{:x}",
        xxhash_rust::xxh3::xxh3_64(root.to_string_lossy().as_bytes())
    );
    Some(dir.join(format!("{key}.bin")))
}

/// Loads cached duplicate groups for the given root directory.
/// Shows a progress bar while validating file existence. Press ESC to skip validation.
pub fn load(root: &Path) -> Option<DuplicateGroups> {
    let path = cache_path_for(root)?;
    let data = fs::read(&path).ok()?;
    let entry: CacheEntry = bincode::deserialize(&data).ok()?;
    if entry.root != root {
        return None;
    }
    let total_files: usize = entry.groups.iter().map(|g| g.paths.len()).sum();
    let bar = indicatif::ProgressBar::new(total_files as u64);
    bar.set_style(
        indicatif::ProgressStyle::default_bar()
            .template("{spinner:.cyan} Validating cached files [{bar:30.cyan/dim}] {pos}/{len} (ESC to skip)")
            .unwrap()
            .progress_chars("█▓░"),
    );

    let cancelled = Arc::new(AtomicBool::new(false));
    let cancelled_clone = Arc::clone(&cancelled);

    let _ = crossterm::terminal::enable_raw_mode();
    let esc_thread = thread::spawn(move || {
        use crossterm::event::{self, Event, KeyCode};
        use std::time::Duration;
        while !cancelled_clone.load(Ordering::Relaxed) {
            if event::poll(Duration::from_millis(50)).unwrap_or(false) {
                if let Ok(Event::Key(key)) = event::read() {
                    if key.code == KeyCode::Esc {
                        cancelled_clone.store(true, Ordering::Relaxed);
                        break;
                    }
                }
            }
        }
    });

    let groups = entry
        .groups
        .into_iter()
        .map(|g| {
            g.paths
                .into_iter()
                .map(|p| {
                    bar.inc(1);
                    if cancelled.load(Ordering::Relaxed) {
                        return FileInfo {
                            size: g.size,
                            created: None,
                            modified: None,
                            path: p,
                        };
                    }
                    let meta = fs::metadata(&p).ok();
                    FileInfo {
                        path: p,
                        size: g.size,
                        created: meta.as_ref().and_then(|m| m.created().ok()),
                        modified: meta.as_ref().and_then(|m| m.modified().ok()),
                    }
                })
                .filter(|f| cancelled.load(Ordering::Relaxed) || f.path.exists())
                .collect::<Vec<_>>()
        })
        .filter(|g| g.len() > 1)
        .collect();

    cancelled.store(true, Ordering::Relaxed);
    let _ = esc_thread.join();
    let _ = crossterm::terminal::disable_raw_mode();
    bar.finish_and_clear();
    Some(groups)
}

/// Saves duplicate groups to the cache for the given root directory.
pub fn save(root: &Path, groups: &DuplicateGroups) {
    let Some(path) = cache_path_for(root) else {
        return;
    };
    let Some(dir) = cache_dir() else { return };
    let _ = fs::create_dir_all(&dir);
    let entry = CacheEntry {
        root: root.to_path_buf(),
        groups: groups
            .iter()
            .map(|g| CachedGroup {
                size: g[0].size,
                paths: g.iter().map(|f| f.path.clone()).collect(),
            })
            .collect(),
    };
    if let Ok(data) = bincode::serialize(&entry) {
        let _ = fs::write(&path, data);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scanner::FileInfo;
    use std::io::Write;
    use tempfile::TempDir;

    fn make_groups(dir: &Path) -> DuplicateGroups {
        vec![vec![
            FileInfo {
                path: dir.join("a.txt"),
                size: 5,
                created: None,
                modified: None,
            },
            FileInfo {
                path: dir.join("b.txt"),
                size: 5,
                created: None,
                modified: None,
            },
        ]]
    }

    #[test]
    fn test_save_load_roundtrip() {
        let dir = TempDir::new().unwrap();
        // Create actual files so load's exists() check passes
        std::fs::File::create(dir.path().join("a.txt"))
            .unwrap()
            .write_all(b"hello")
            .unwrap();
        std::fs::File::create(dir.path().join("b.txt"))
            .unwrap()
            .write_all(b"hello")
            .unwrap();

        let groups = make_groups(dir.path());
        save(dir.path(), &groups);
        let loaded = load(dir.path()).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].len(), 2);
        assert_eq!(loaded[0][0].size, 5);
    }

    #[test]
    fn test_load_nonexistent() {
        let dir = TempDir::new().unwrap();
        let result = load(dir.path());
        assert!(result.is_none());
    }

    #[test]
    fn test_load_filters_missing_files() {
        let dir = TempDir::new().unwrap();
        // Create only one file — the other is "missing"
        std::fs::File::create(dir.path().join("a.txt"))
            .unwrap()
            .write_all(b"hello")
            .unwrap();

        let groups = make_groups(dir.path());
        save(dir.path(), &groups);
        let loaded = load(dir.path());
        // Group filtered out because only 1 file exists (< 2)
        assert!(loaded.unwrap().is_empty());
    }

    #[test]
    fn test_save_multiple_groups() {
        let dir = TempDir::new().unwrap();
        std::fs::File::create(dir.path().join("a.txt"))
            .unwrap()
            .write_all(b"hello")
            .unwrap();
        std::fs::File::create(dir.path().join("b.txt"))
            .unwrap()
            .write_all(b"hello")
            .unwrap();
        std::fs::File::create(dir.path().join("c.txt"))
            .unwrap()
            .write_all(b"world!")
            .unwrap();
        std::fs::File::create(dir.path().join("d.txt"))
            .unwrap()
            .write_all(b"world!")
            .unwrap();

        let groups = vec![
            vec![
                FileInfo {
                    path: dir.path().join("a.txt"),
                    size: 5,
                    created: None,
                    modified: None,
                },
                FileInfo {
                    path: dir.path().join("b.txt"),
                    size: 5,
                    created: None,
                    modified: None,
                },
            ],
            vec![
                FileInfo {
                    path: dir.path().join("c.txt"),
                    size: 6,
                    created: None,
                    modified: None,
                },
                FileInfo {
                    path: dir.path().join("d.txt"),
                    size: 6,
                    created: None,
                    modified: None,
                },
            ],
        ];
        save(dir.path(), &groups);
        let loaded = load(dir.path()).unwrap();
        assert_eq!(loaded.len(), 2);
    }
}
