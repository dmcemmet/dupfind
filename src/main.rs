mod app;
mod cache;
mod preview;
mod scanner;
mod tree;

use std::io::Write;
use std::path::{Path, PathBuf};
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
    /// Directory to scan for duplicates
    path: PathBuf,

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
}

fn main() -> std::io::Result<()> {
    let cli = Cli::parse();
    let root = cli.path.canonicalize().unwrap_or(cli.path);

    // Check for existing scan state (incomplete or complete)
    eprintln!("Checking state for {}...", root.display());
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
            "r" | "rescan" => run_scan(&root, None, &cli.exclude),
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
                "r" | "rescan" => run_scan(&root, None, &cli.exclude),
                "q" => return Ok(()),
                _ => run_scan(&root, Some(state), &cli.exclude),
            }
        } else {
            eprint!("Found incomplete scan ({summary}). [R]esume / [f]resh / [q]uit: ");
            std::io::stderr().flush()?;
            let mut input = String::new();
            std::io::stdin().read_line(&mut input)?;
            match input.trim().to_lowercase().as_str() {
                "f" | "fresh" => run_scan(&root, None, &cli.exclude),
                "q" => return Ok(()),
                _ => run_scan(&root, Some(state), &cli.exclude),
            }
        }
    } else {
        run_scan(&root, None, &cli.exclude)
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
                let rel = f.path.strip_prefix(&root).unwrap_or(&f.path);
                eprintln!("  {}", rel.display());
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
    root: &Path,
    resume: Option<ScanState>,
    exclude: &[String],
) -> scanner::DuplicateGroups {
    let progress = Arc::new(scanner::ScanProgress::new());
    let prog = Arc::clone(&progress);
    let scan_root = root.to_path_buf();
    let exclude_patterns: Vec<String> = exclude.to_vec();

    let handle = thread::spawn(move || {
        scanner::scan_duplicates(&scan_root, &prog, resume, &exclude_patterns)
    });

    let bar_style = ProgressStyle::default_bar()
        .template("{spinner:.cyan} {msg} [{bar:40.cyan/dim}] {pos}/{len} ({eta})")
        .unwrap()
        .progress_chars("█▓░");

    let spinner_style = ProgressStyle::default_spinner()
        .template("{spinner:.cyan} {msg}")
        .unwrap();

    let mut pb: Option<ProgressBar> = None;
    let mut current_stage = 255u8;

    loop {
        let stage = progress.stage.load(Ordering::Relaxed);
        if stage >= 3 {
            break;
        }

        match stage {
            0 => {
                // Walk phase
                let dirs_total = progress.dirs_total.load(Ordering::Relaxed) as u64;
                let dirs_done = progress.dirs_done.load(Ordering::Relaxed) as u64;
                let files = progress.files_found.load(Ordering::Relaxed);

                if dirs_done > 0 && current_stage != 0 {
                    if let Some(bar) = pb.take() {
                        bar.finish_and_clear();
                    }
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
                        if dirs_done > 0 {
                            // Switch to bar
                            bar.finish_and_clear();
                        }
                    } else {
                        bar.set_length(dirs_total);
                        bar.set_position(dirs_done);
                        bar.set_message(format!("Walking ({files} files)"));
                    }
                }
                // Handle transition from spinner to bar
                if current_stage == 254 && dirs_done > 0 {
                    if let Some(b) = pb.take() {
                        b.finish_and_clear();
                    }
                    let bar = ProgressBar::new(dirs_total);
                    bar.set_style(bar_style.clone());
                    bar.set_message(format!("Walking ({files} files)"));
                    bar.set_position(dirs_done);
                    pb = Some(bar);
                    current_stage = 0;
                }
            }
            1 => {
                // Partial hash phase
                if current_stage != 1 {
                    if let Some(bar) = pb.take() {
                        bar.finish_and_clear();
                    }
                    let total = progress.to_hash.load(Ordering::Relaxed) as u64;
                    let bar = ProgressBar::new(total);
                    bar.set_style(bar_style.clone());
                    bar.set_message("Partial hashing");
                    pb = Some(bar);
                    current_stage = 1;
                }
                if let Some(bar) = &pb {
                    bar.set_length(progress.to_hash.load(Ordering::Relaxed) as u64);
                    bar.set_position(progress.hashed.load(Ordering::Relaxed) as u64);
                }
            }
            2 => {
                // Full hash phase
                if current_stage != 2 {
                    if let Some(bar) = pb.take() {
                        bar.finish_and_clear();
                    }
                    let total = progress.to_hash.load(Ordering::Relaxed) as u64;
                    let bar = ProgressBar::new(total);
                    bar.set_style(bar_style.clone());
                    bar.set_message("Full hashing   ");
                    pb = Some(bar);
                    current_stage = 2;
                }
                if let Some(bar) = &pb {
                    bar.set_length(progress.to_hash.load(Ordering::Relaxed) as u64);
                    bar.set_position(progress.hashed.load(Ordering::Relaxed) as u64);
                }
            }
            _ => break,
        }
        thread::sleep(Duration::from_millis(50));
    }

    if let Some(bar) = pb.take() {
        bar.finish_and_clear();
    }

    let (groups, _state) = handle.join().expect("scan thread panicked");

    let total_files = progress.files_found.load(Ordering::Relaxed);
    eprintln!(
        "Scanned {total_files} files. Found {} groups of duplicates.",
        groups.len()
    );

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
