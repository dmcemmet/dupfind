mod app;
mod cache;
mod preview;
mod scanner;
mod tree;

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

use clap::Parser;
use indicatif::{ProgressBar, ProgressStyle};

use scanner::ScanState;

#[derive(Parser)]
#[command(
    name = "dupfinder",
    about = "Find duplicate files with a TUI interface"
)]
struct Cli {
    /// Directories to scan for duplicates
    #[arg(required = true)]
    paths: Vec<PathBuf>,

    /// Read-only mode: disable all file deletion
    #[arg(long = "ro")]
    read_only: bool,

    /// Exclude directories matching glob patterns (e.g. --exclude "@*" "tmp")
    #[arg(short = 'e', long = "exclude")]
    exclude: Vec<String>,

    /// Minimum file size to consider (e.g. "1MB", "500KB", "1024")
    #[arg(long = "min-size")]
    min_size: Option<String>,

    /// Dry-run: show what would be deleted without launching TUI
    #[arg(long = "dry-run")]
    dry_run: bool,

    /// Fast mode: skip full hash, sample N chunks from the middle (default: 1)
    #[arg(long = "fast", num_args = 0..=1, default_missing_value = "1")]
    fast: Option<u32>,
}

fn main() -> std::io::Result<()> {
    let cli = Cli::parse();
    let roots: Vec<PathBuf> = cli.paths.iter()
        .map(|p| p.canonicalize().unwrap_or(p.clone()))
        .collect();
    let root = roots[0].clone(); // primary root for cache/state/display

    // Check for existing scan state (incomplete or complete)
    eprintln!("Checking state for {}...", roots.iter().map(|r| r.display().to_string()).collect::<Vec<_>>().join(", "));
    let resume_state = ScanState::load(&root);
    let cached_results = cache::load(&root);

    let mut groups = if let Some(cached) = cached_results {
        let file_count: usize = cached.iter().map(|g| g.len()).sum();
        let group_count = cached.len();
        eprint!(
            "Found cached results ({group_count} groups, {file_count} files). [U]se cache / [r]escan / [q]uit: ",
        );
        std::io::stderr().flush()?;
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        match input.trim().to_lowercase().as_str() {
            "r" | "rescan" => run_scan(&roots, None, &cli.exclude, cli.fast),
            "q" | "n" => return Ok(()),
            _ => cached,
        }
    } else if let Some(state) = resume_state {
        let summary = state.status_summary();
        if state.is_done() {
            eprint!("Found completed scan ({summary}). [U]se / [r]escan / [q]uit: ");
            std::io::stderr().flush()?;
            let mut input = String::new();
            std::io::stdin().read_line(&mut input)?;
            match input.trim().to_lowercase().as_str() {
                "r" | "rescan" => run_scan(&roots, None, &cli.exclude, cli.fast),
                "q" => return Ok(()),
                _ => run_scan(&roots, Some(state), &cli.exclude, cli.fast),
            }
        } else {
            eprint!("Found incomplete scan ({summary}). [R]esume / [f]resh / [q]uit: ");
            std::io::stderr().flush()?;
            let mut input = String::new();
            std::io::stdin().read_line(&mut input)?;
            match input.trim().to_lowercase().as_str() {
                "f" | "fresh" => run_scan(&roots, None, &cli.exclude, cli.fast),
                "q" => return Ok(()),
                _ => run_scan(&roots, Some(state), &cli.exclude, cli.fast),
            }
        }
    } else {
        run_scan(&roots, None, &cli.exclude, cli.fast)
    };

    // Save final results to cache
    cache::save(&root, &groups);

    // Apply min-size filter
    if let Some(ref min_size_str) = cli.min_size {
        let min_bytes = parse_size(min_size_str);
        groups.retain(|g| g[0].size >= min_bytes);
    }

    if groups.is_empty() {
        eprintln!("No duplicates found.");
        return Ok(());
    }

    // Dry-run mode: print report and exit
    if cli.dry_run {
        let mut total_wasted: u64 = 0;
        for (i, group) in groups.iter().enumerate() {
            let size = group[0].size;
            eprintln!("Group {} ({}):", i + 1, format_size_simple(size));
            for f in group {
                if roots.len() == 1 {
                    let rel = f.path.strip_prefix(&root).unwrap_or(&f.path);
                    eprintln!("  {}", rel.display());
                } else {
                    eprintln!("  {}", f.path.display());
                }
            }
            total_wasted += size * (group.len() as u64 - 1);
        }
        eprintln!("\n{} groups, {} wasted space", groups.len(), format_size_simple(total_wasted));
        return Ok(());
    }

    let mut terminal = ratatui::init();
    let mut app = app::App::new(groups, root, cli.read_only);
    let result = app.run(&mut terminal);
    ratatui::restore();
    result
}

fn run_scan(
    roots: &[PathBuf],
    resume: Option<ScanState>,
    exclude: &[String],
    fast: Option<u32>,
) -> scanner::DuplicateGroups {
    let progress = Arc::new(scanner::ScanProgress::new());
    let prog = Arc::clone(&progress);
    let scan_roots = roots.to_vec();
    let exclude_patterns: Vec<String> = exclude.to_vec();

    let handle = thread::spawn(move || {
        scanner::scan_duplicates(&scan_roots, &prog, resume, &exclude_patterns, fast)
    });

    let bar_style = ProgressStyle::default_bar()
        .template("{spinner:.cyan} {prefix} [{bar:40.cyan/dim}] {pos}/{len} {msg}")
        .unwrap()
        .progress_chars("█▓░");

    let spinner_style = ProgressStyle::default_spinner()
        .template("{spinner:.cyan} {msg}")
        .unwrap();

    let mut pb: Option<ProgressBar> = None;
    let mut current_stage = 255u8;

    loop {
        let stage = progress.stage.load(Ordering::Relaxed);
        if stage >= 4 {
            break;
        }

        match stage {
            0 => {
                let dirs_total = progress.dirs_total.load(Ordering::Relaxed) as u64;
                let dirs_done = progress.dirs_done.load(Ordering::Relaxed) as u64;
                let files = progress.files_found.load(Ordering::Relaxed);

                if dirs_done > 0 && current_stage != 0 {
                    if let Some(bar) = pb.take() { bar.finish_and_clear(); }
                    let bar = ProgressBar::new(dirs_total);
                    bar.set_style(bar_style.clone());
                    pb = Some(bar);
                    current_stage = 0;
                } else if current_stage == 255 {
                    let bar = ProgressBar::new_spinner();
                    bar.set_style(spinner_style.clone());
                    bar.enable_steady_tick(Duration::from_millis(80));
                    pb = Some(bar);
                    current_stage = 254;
                }

                if let Some(bar) = &pb {
                    if current_stage == 254 {
                        bar.set_message(format!("Collecting directories... {dirs_total} found"));
                        if dirs_done > 0 { bar.finish_and_clear(); }
                    } else {
                        bar.set_length(dirs_total);
                        bar.set_position(dirs_done);
                        bar.set_prefix("Walking");
                        bar.set_message(format!("({files} files)"));
                    }
                }
                if current_stage == 254 && dirs_done > 0 {
                    if let Some(b) = pb.take() { b.finish_and_clear(); }
                    let bar = ProgressBar::new(dirs_total);
                    bar.set_style(bar_style.clone());
                    bar.set_prefix("Walking");
                    bar.set_message(format!("({files} files)"));
                    bar.set_position(dirs_done);
                    pb = Some(bar);
                    current_stage = 0;
                }
            }
            1 => {
                if current_stage != 1 {
                    if let Some(bar) = pb.take() { bar.finish_with_message("✓"); }
                    let total = progress.to_hash.load(Ordering::Relaxed) as u64;
                    let bar = ProgressBar::new(total);
                    bar.set_style(bar_style.clone());
                    bar.set_prefix("Head hash");
                    pb = Some(bar);
                    current_stage = 1;
                }
                if let Some(bar) = &pb {
                    let total = progress.to_hash.load(Ordering::Relaxed);
                    bar.set_length(total as u64);
                    bar.set_position(progress.hashed.load(Ordering::Relaxed) as u64);
                    bar.set_message(format!("({total} candidates)"));
                }
            }
            2 => {
                if current_stage != 2 {
                    if let Some(bar) = pb.take() { bar.finish_with_message("✓"); }
                    let total = progress.to_hash.load(Ordering::Relaxed) as u64;
                    let bar = ProgressBar::new(total);
                    bar.set_style(bar_style.clone());
                    bar.set_prefix("Tail hash");
                    pb = Some(bar);
                    current_stage = 2;
                }
                if let Some(bar) = &pb {
                    let total = progress.to_hash.load(Ordering::Relaxed);
                    bar.set_length(total as u64);
                    bar.set_position(progress.hashed.load(Ordering::Relaxed) as u64);
                    bar.set_message(format!("({total} candidates)"));
                }
            }
            3 => {
                let dupes = progress.dupes_found.load(Ordering::Relaxed);
                let groups = progress.groups_found.load(Ordering::Relaxed);
                if current_stage != 3 {
                    if let Some(bar) = pb.take() { bar.finish_with_message("✓"); }
                    let total = progress.to_hash.load(Ordering::Relaxed) as u64;
                    let bar = ProgressBar::new(total);
                    bar.set_style(bar_style.clone());
                    bar.set_prefix("Verifying");
                    pb = Some(bar);
                    current_stage = 3;
                }
                if let Some(bar) = &pb {
                    bar.set_length(progress.to_hash.load(Ordering::Relaxed) as u64);
                    bar.set_position(progress.hashed.load(Ordering::Relaxed) as u64);
                    bar.set_message(format!("({groups} groups, {dupes} dupes)"));
                }
            }
            _ => break,
        }
        thread::sleep(Duration::from_millis(50));
    }

    if let Some(bar) = pb.take() { bar.finish_with_message("✓"); }

    let (groups, _state) = handle.join().expect("scan thread panicked");

    let total_files = progress.files_found.load(Ordering::Relaxed);
    let dupe_files: usize = groups.iter().map(|g| g.len()).sum();
    eprintln!("Scanned {total_files} files. Found {} groups ({dupe_files} duplicate files).", groups.len());

    groups
}

fn parse_size(s: &str) -> u64 {
    let s = s.trim().to_uppercase();
    let (num, mult) = if s.ends_with("GB") {
        (&s[..s.len()-2], 1024 * 1024 * 1024)
    } else if s.ends_with("MB") {
        (&s[..s.len()-2], 1024 * 1024)
    } else if s.ends_with("KB") {
        (&s[..s.len()-2], 1024)
    } else if s.ends_with("B") {
        (&s[..s.len()-1], 1)
    } else {
        (s.as_str(), 1)
    };
    num.trim().parse::<u64>().unwrap_or(0) * mult
}

fn format_size_simple(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    for unit in UNITS {
        if size < 1024.0 {
            return format!("{size:.1} {unit}");
        }
        size /= 1024.0;
    }
    format!("{size:.1} PB")
}
