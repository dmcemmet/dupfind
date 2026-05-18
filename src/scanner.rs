use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};
use walkdir::WalkDir;
use xxhash_rust::xxh3::{Xxh3, xxh3_64};

use serde::{Deserialize, Serialize};

const PARTIAL_HASH_SIZE: u64 = 4096;

/// Information about a single file discovered during scanning.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileInfo {
    pub path: PathBuf,
    pub size: u64,
    pub created: Option<SystemTime>,
    pub modified: Option<SystemTime>,
}

/// Groups of files that are byte-for-byte duplicates of each other.
pub type DuplicateGroups = Vec<Vec<FileInfo>>;

/// Current phase of the scanning process.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Default, Debug)]
pub enum ScanPhase {
    #[default]
    Walking,
    PartialHashing,
    FullHashing,
    Done,
}

/// Per-file entry: path + size collected during walk.
#[derive(Serialize, Deserialize, Clone)]
pub struct FileEntry {
    pub path: PathBuf,
    pub size: u64,
}

/// Incremental scan state persisted to disk.
#[derive(Serialize, Deserialize, Default)]
pub struct ScanState {
    pub root: PathBuf,
    pub phase: ScanPhase,
    /// Directories completed during walk phase.
    pub completed_dirs: HashSet<PathBuf>,
    /// All files discovered (walk phase output).
    pub files: Vec<FileEntry>,
    /// Files that need partial hashing (same-size groups, >1 file).
    pub partial_candidates: Vec<FileEntry>,
    /// Files that passed partial hash (partial hash phase output).
    /// Stored as (path, size, partial_hash).
    pub partial_results: Vec<(PathBuf, u64, u64)>,
    /// Index of next file to partial-hash (for resume).
    pub partial_idx: usize,
}

impl ScanState {
    fn state_path(root: &Path) -> Option<PathBuf> {
        let dir = dirs::home_dir()?.join(".dupfinder");
        let key = format!("{:x}", xxh3_64(root.to_string_lossy().as_bytes()));
        Some(dir.join(format!("{key}.scan")))
    }

    pub fn load(root: &Path) -> Option<Self> {
        let path = Self::state_path(root)?;
        let data = fs::read(&path).ok()?;
        let state: Self = bincode::deserialize(&data).ok()?;
        if state.root == root {
            Some(state)
        } else {
            None
        }
    }

    pub fn save(&self) {
        let Some(path) = Self::state_path(&self.root) else {
            return;
        };
        let Some(dir) = dirs::home_dir().map(|h| h.join(".dupfinder")) else {
            return;
        };
        let _ = fs::create_dir_all(&dir);
        if let Ok(data) = bincode::serialize(self) {
            let _ = fs::write(&path, data);
        }
    }

    pub fn status_summary(&self) -> String {
        match self.phase {
            ScanPhase::Walking => format!(
                "{} dirs, {} files",
                self.completed_dirs.len(),
                self.files.len()
            ),
            ScanPhase::PartialHashing => format!(
                "partial hash {}/{}",
                self.partial_idx,
                self.partial_candidates.len()
            ),
            ScanPhase::FullHashing | ScanPhase::Done => {
                format!("{} files, phase: {:?}", self.files.len(), self.phase)
            }
        }
    }

    pub fn is_done(&self) -> bool {
        self.phase == ScanPhase::Done
    }
}

/// Progress counters shared with the UI thread.
pub struct ScanProgress {
    pub files_found: AtomicUsize,
    pub dirs_done: AtomicUsize,
    pub dirs_total: AtomicUsize,
    /// 0=walk, 1=partial hash, 2=full hash, 3=done
    pub stage: AtomicUsize,
    pub hashed: AtomicUsize,
    pub to_hash: AtomicUsize,
}

impl ScanProgress {
    pub fn new() -> Self {
        Self {
            files_found: AtomicUsize::new(0),
            dirs_done: AtomicUsize::new(0),
            dirs_total: AtomicUsize::new(0),
            stage: AtomicUsize::new(0),
            hashed: AtomicUsize::new(0),
            to_hash: AtomicUsize::new(0),
        }
    }
}

/// 3-pass scan: walk → partial hash → full hash. Resumable at each stage.
pub fn scan_duplicates(
    root: &Path,
    progress: &ScanProgress,
    mut state: Option<ScanState>,
    exclude: &[String],
) -> (DuplicateGroups, ScanState) {
    let mut s = state.take().unwrap_or_else(|| ScanState {
        root: root.to_path_buf(),
        ..Default::default()
    });

    let save_interval = Duration::from_secs(60);

    // === Pass 1: Walk directories, collect file paths + sizes ===
    if s.phase == ScanPhase::Walking {
        progress.stage.store(0, Ordering::Relaxed);
        progress.files_found.store(s.files.len(), Ordering::Relaxed);
        progress
            .dirs_done
            .store(s.completed_dirs.len(), Ordering::Relaxed);

        let mut all_dirs: Vec<PathBuf> = Vec::new();
        let walker = WalkDir::new(root).follow_links(false).into_iter();
        for entry in walker
            .filter_entry(|e| {
                if !e.file_type().is_dir() {
                    return true;
                }
                let name = e.file_name().to_string_lossy();
                !exclude.iter().any(|pat| glob_match(pat, &name))
            })
            .filter_map(|e| e.ok())
        {
            if entry.file_type().is_dir() {
                all_dirs.push(entry.into_path());
                progress.dirs_total.store(all_dirs.len(), Ordering::Relaxed);
            }
        }

        let mut last_save = Instant::now();
        for dir in &all_dirs {
            if s.completed_dirs.contains(dir) {
                continue;
            }
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.filter_map(|e| e.ok()) {
                    if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                        continue;
                    }
                    if let Ok(meta) = entry.metadata() {
                        let size = meta.len();
                        if size > 0 {
                            s.files.push(FileEntry {
                                path: entry.path(),
                                size,
                            });
                            progress.files_found.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }
            s.completed_dirs.insert(dir.clone());
            progress.dirs_done.fetch_add(1, Ordering::Relaxed);

            if last_save.elapsed() >= save_interval {
                s.save();
                last_save = Instant::now();
            }
        }

        // Build partial hash candidates: files with non-unique sizes
        let mut by_size: HashMap<u64, Vec<FileEntry>> = HashMap::new();
        for f in &s.files {
            by_size.entry(f.size).or_default().push(f.clone());
        }
        s.partial_candidates = by_size
            .into_values()
            .filter(|g| g.len() > 1)
            .flatten()
            .collect();
        s.phase = ScanPhase::PartialHashing;
        s.save();
    }

    // === Pass 2: Partial hash (first+last 4KB) on size-collision candidates ===
    if s.phase == ScanPhase::PartialHashing {
        progress.stage.store(1, Ordering::Relaxed);
        progress
            .to_hash
            .store(s.partial_candidates.len(), Ordering::Relaxed);
        progress.hashed.store(s.partial_idx, Ordering::Relaxed);

        let mut last_save = Instant::now();
        while s.partial_idx < s.partial_candidates.len() {
            let f = &s.partial_candidates[s.partial_idx];
            if let Some(h) = partial_hash(&f.path, f.size) {
                s.partial_results.push((f.path.clone(), f.size, h));
            }
            s.partial_idx += 1;
            progress.hashed.fetch_add(1, Ordering::Relaxed);

            if last_save.elapsed() >= save_interval {
                s.save();
                last_save = Instant::now();
            }
        }

        s.phase = ScanPhase::FullHashing;
        s.save();
    }

    // === Pass 3: Full hash on partial-hash collision candidates ===
    progress.stage.store(2, Ordering::Relaxed);

    // Group by (size, partial_hash)
    let mut by_key: HashMap<(u64, u64), Vec<(PathBuf, u64)>> = HashMap::new();
    for (path, size, ph) in &s.partial_results {
        by_key
            .entry((*size, *ph))
            .or_default()
            .push((path.clone(), *size));
    }
    let full_candidates: Vec<Vec<(PathBuf, u64)>> =
        by_key.into_values().filter(|g| g.len() > 1).collect();

    let full_count: usize = full_candidates
        .iter()
        .filter(|g| g[0].1 > PARTIAL_HASH_SIZE * 2)
        .map(|g| g.len())
        .sum();
    progress.hashed.store(0, Ordering::Relaxed);
    progress.to_hash.store(full_count, Ordering::Relaxed);

    let groups: DuplicateGroups = full_candidates
        .into_par_iter()
        .flat_map(|group| {
            let size = group[0].1;
            if size <= PARTIAL_HASH_SIZE * 2 {
                // Partial hash == full hash for small files
                return vec![
                    group
                        .into_iter()
                        .map(|(p, sz)| make_file_info(p, sz))
                        .collect(),
                ];
            }
            let mut by_hash: HashMap<u64, Vec<FileInfo>> = HashMap::new();
            for (path, sz) in &group {
                if let Some(h) = full_hash(path) {
                    by_hash
                        .entry(h)
                        .or_default()
                        .push(make_file_info(path.clone(), *sz));
                }
                progress.hashed.fetch_add(1, Ordering::Relaxed);
            }
            by_hash
                .into_values()
                .filter(|g| g.len() > 1)
                .collect::<Vec<_>>()
        })
        .collect();

    s.phase = ScanPhase::Done;
    s.save();
    progress.stage.store(3, Ordering::Relaxed);
    (groups, s)
}

fn make_file_info(path: PathBuf, size: u64) -> FileInfo {
    let meta = fs::metadata(&path).ok();
    FileInfo {
        path,
        size,
        created: meta.as_ref().and_then(|m| m.created().ok()),
        modified: meta.as_ref().and_then(|m| m.modified().ok()),
    }
}

fn partial_hash(path: &Path, size: u64) -> Option<u64> {
    let mut file = File::open(path).ok()?;
    let mut buf = vec![0u8; PARTIAL_HASH_SIZE as usize];

    let head_len = std::cmp::min(size, PARTIAL_HASH_SIZE) as usize;
    file.read_exact(&mut buf[..head_len]).ok()?;
    let mut data = buf[..head_len].to_vec();

    if size > PARTIAL_HASH_SIZE {
        file.seek(SeekFrom::End(-(PARTIAL_HASH_SIZE as i64))).ok()?;
        let mut tail = vec![0u8; PARTIAL_HASH_SIZE as usize];
        file.read_exact(&mut tail).ok()?;
        data.extend_from_slice(&tail);
    }

    Some(xxh3_64(&data))
}

fn full_hash(path: &Path) -> Option<u64> {
    let mut file = File::open(path).ok()?;
    let mut buf = vec![0u8; 1 << 16];
    let mut hasher = Xxh3::new();
    loop {
        let n = file.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Some(hasher.digest())
}

/// Simple glob matching supporting * and ? wildcards.
fn glob_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    let (mut pi, mut ni) = (0, 0);
    let (mut star_p, mut star_n) = (usize::MAX, 0);

    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star_p = pi;
            star_n = ni;
            pi += 1;
        } else if star_p != usize::MAX {
            pi = star_p + 1;
            star_n += 1;
            ni = star_n;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn test_scan(path: &Path) -> DuplicateGroups {
        let progress = ScanProgress::new();
        let (groups, _) = scan_duplicates(path, &progress, None, &[]);
        groups
    }

    #[test]
    fn test_finds_duplicates() {
        let dir = TempDir::new().unwrap();
        let content = b"hello world duplicate content";

        let f1 = dir.path().join("a.txt");
        let f2 = dir.path().join("sub");
        fs::create_dir_all(&f2).unwrap();
        let f2 = f2.join("b.txt");

        File::create(&f1).unwrap().write_all(content).unwrap();
        File::create(&f2).unwrap().write_all(content).unwrap();

        let f3 = dir.path().join("unique.txt");
        File::create(&f3).unwrap().write_all(b"unique").unwrap();

        let groups = test_scan(dir.path());
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].len(), 2);

        let paths: Vec<_> = groups[0].iter().map(|f| f.path.clone()).collect();
        assert!(paths.contains(&f1));
        assert!(paths.contains(&f2));
    }

    #[test]
    fn test_no_duplicates() {
        let dir = TempDir::new().unwrap();
        File::create(dir.path().join("a.txt"))
            .unwrap()
            .write_all(b"aaa")
            .unwrap();
        File::create(dir.path().join("b.txt"))
            .unwrap()
            .write_all(b"bbb")
            .unwrap();
        let groups = test_scan(dir.path());
        assert!(groups.is_empty());
    }

    #[test]
    fn test_empty_files_ignored() {
        let dir = TempDir::new().unwrap();
        File::create(dir.path().join("a.txt")).unwrap();
        File::create(dir.path().join("b.txt")).unwrap();
        let groups = test_scan(dir.path());
        assert!(groups.is_empty());
    }

    #[test]
    fn test_multiple_duplicate_groups() {
        let dir = TempDir::new().unwrap();
        File::create(dir.path().join("a1.txt"))
            .unwrap()
            .write_all(b"group_a")
            .unwrap();
        File::create(dir.path().join("a2.txt"))
            .unwrap()
            .write_all(b"group_a")
            .unwrap();
        File::create(dir.path().join("b1.txt"))
            .unwrap()
            .write_all(b"group_b")
            .unwrap();
        File::create(dir.path().join("b2.txt"))
            .unwrap()
            .write_all(b"group_b")
            .unwrap();
        let groups = test_scan(dir.path());
        assert_eq!(groups.len(), 2);
    }

    #[test]
    fn test_large_file_partial_hash() {
        let dir = TempDir::new().unwrap();
        let content: Vec<u8> = (0..10000).map(|i| (i % 256) as u8).collect();

        File::create(dir.path().join("large1.bin"))
            .unwrap()
            .write_all(&content)
            .unwrap();
        File::create(dir.path().join("large2.bin"))
            .unwrap()
            .write_all(&content)
            .unwrap();

        let mut diff = content.clone();
        diff[5000] = 255;
        File::create(dir.path().join("large3.bin"))
            .unwrap()
            .write_all(&diff)
            .unwrap();

        let groups = test_scan(dir.path());
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].len(), 2);
    }

    #[test]
    fn test_resume_from_walk_phase() {
        let dir = TempDir::new().unwrap();
        let content = b"duplicate content here!";
        let sub = dir.path().join("sub");
        fs::create_dir_all(&sub).unwrap();
        File::create(dir.path().join("a.txt"))
            .unwrap()
            .write_all(content)
            .unwrap();
        File::create(sub.join("b.txt"))
            .unwrap()
            .write_all(content)
            .unwrap();

        let progress = ScanProgress::new();
        let (g1, state) = scan_duplicates(dir.path(), &progress, None, &[]);

        // Resume with completed state — should skip walk and partial, just do full hash
        let progress2 = ScanProgress::new();
        let (g2, _) = scan_duplicates(dir.path(), &progress2, Some(state), &[]);

        assert_eq!(g1.len(), g2.len());
    }

    #[test]
    fn test_exclude_pattern() {
        let dir = TempDir::new().unwrap();
        let content = b"same content";
        let excluded = dir.path().join("@Recycle");
        fs::create_dir_all(&excluded).unwrap();
        File::create(excluded.join("a.txt"))
            .unwrap()
            .write_all(content)
            .unwrap();
        File::create(dir.path().join("b.txt"))
            .unwrap()
            .write_all(content)
            .unwrap();

        let groups = {
            let progress = ScanProgress::new();
            let (g, _) = scan_duplicates(dir.path(), &progress, None, &["@*".to_string()]);
            g
        };
        // Only one file found (the other is in excluded dir), so no duplicates
        assert!(groups.is_empty());
    }

    // --- glob_match tests ---

    #[test]
    fn test_glob_exact_match() {
        assert!(glob_match("hello", "hello"));
        assert!(!glob_match("hello", "world"));
    }

    #[test]
    fn test_glob_star_wildcard() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("*.txt", "file.txt"));
        assert!(!glob_match("*.txt", "file.rs"));
        assert!(glob_match("file*", "filename"));
        assert!(glob_match("*name*", "filename.txt"));
    }

    #[test]
    fn test_glob_question_mark() {
        assert!(glob_match("?", "a"));
        assert!(!glob_match("?", "ab"));
        assert!(glob_match("f??e", "file"));
        assert!(!glob_match("f??e", "flame"));
    }

    #[test]
    fn test_glob_combined() {
        assert!(glob_match("*.t?t", "file.txt"));
        assert!(!glob_match("*.t?t", "file.text"));
        assert!(glob_match("@*", "@Recycle"));
        assert!(glob_match(".*", ".git"));
    }

    #[test]
    fn test_glob_empty() {
        assert!(glob_match("", ""));
        assert!(!glob_match("", "a"));
        assert!(glob_match("*", ""));
    }

    // --- partial_hash tests ---

    #[test]
    fn test_partial_hash_small_file() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("small.bin");
        File::create(&p).unwrap().write_all(b"tiny").unwrap();
        let h = partial_hash(&p, 4);
        assert!(h.is_some());
    }

    #[test]
    fn test_partial_hash_exactly_4096() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("exact.bin");
        let data = vec![0xABu8; 4096];
        File::create(&p).unwrap().write_all(&data).unwrap();
        let h = partial_hash(&p, 4096);
        assert!(h.is_some());
    }

    #[test]
    fn test_partial_hash_large_file() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("large.bin");
        let data = vec![0x42u8; 10000];
        File::create(&p).unwrap().write_all(&data).unwrap();
        let h = partial_hash(&p, 10000);
        assert!(h.is_some());
    }

    #[test]
    fn test_partial_hash_nonexistent() {
        let h = partial_hash(Path::new("/nonexistent/file.bin"), 100);
        assert!(h.is_none());
    }

    #[test]
    fn test_partial_hash_different_middle() {
        // Two files identical in first/last 4096 bytes but different in middle
        // should produce the same partial hash
        let dir = TempDir::new().unwrap();
        let data1 = vec![0u8; 16000];
        let mut data2 = vec![0u8; 16000];
        data2[8000] = 0xFF; // differ only in middle
        File::create(dir.path().join("a.bin"))
            .unwrap()
            .write_all(&data1)
            .unwrap();
        File::create(dir.path().join("b.bin"))
            .unwrap()
            .write_all(&data2)
            .unwrap();
        let h1 = partial_hash(&dir.path().join("a.bin"), 16000).unwrap();
        let h2 = partial_hash(&dir.path().join("b.bin"), 16000).unwrap();
        assert_eq!(h1, h2);
    }

    // --- phase transition tests ---

    #[test]
    fn test_scan_state_phases() {
        let dir = TempDir::new().unwrap();
        let content = b"duplicate data here";
        File::create(dir.path().join("x.txt"))
            .unwrap()
            .write_all(content)
            .unwrap();
        File::create(dir.path().join("y.txt"))
            .unwrap()
            .write_all(content)
            .unwrap();

        let progress = ScanProgress::new();
        let (_, state) = scan_duplicates(dir.path(), &progress, None, &[]);
        assert_eq!(state.phase, ScanPhase::Done);
        assert!(state.is_done());
    }

    #[test]
    fn test_scan_state_status_summary() {
        let mut s = ScanState::default();
        s.phase = ScanPhase::Walking;
        assert!(s.status_summary().contains("dirs"));

        s.phase = ScanPhase::PartialHashing;
        s.partial_candidates = vec![FileEntry {
            path: PathBuf::from("a"),
            size: 1,
        }];
        assert!(s.status_summary().contains("partial hash"));

        s.phase = ScanPhase::Done;
        assert!(s.status_summary().contains("Done"));
    }
}
