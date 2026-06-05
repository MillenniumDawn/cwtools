use std::path::PathBuf;
use std::time::Instant;

#[test]
fn bench_millennium_dawn_mod() {
    let dirs = vec![
        "/mnt/Linux/Millennium-Dawn/common/countries",
        "/mnt/Linux/Millennium-Dawn/common/ideas",
        "/mnt/Linux/Millennium-Dawn/common/national_focus",
        "/mnt/Linux/Millennium-Dawn/common/decisions",
        "/mnt/Linux/Millennium-Dawn/events",
        "/mnt/Linux/Millennium-Dawn/history",
    ];

    let mut total_files = 0usize;
    let mut total_leaves = 0usize;
    let start = Instant::now();

    for dir in &dirs {
        let config = cwtools_file_manager::file_manager::FileManagerConfig {
            root: PathBuf::from(dir),
            include_dirs: vec![".".into()],
            file_patterns: vec!["*.txt".into()],
            exclude_patterns: vec![],
            ..Default::default()
        };
        let mut manager = cwtools_file_manager::file_manager::FileManager::new(config);
        match manager.discover_and_parse() {
            Ok(files) => {
                let leaves: usize = files.iter().map(|f| f.arena.leaves.len()).sum();
                let file_count = files.len();
                total_files += file_count;
                total_leaves += leaves;
                println!("  {}: {} files, {} leaves", dir, file_count, leaves);
            }
            Err(e) => {
                eprintln!("Error in {}: {}", dir, e);
            }
        }
    }

    let elapsed = start.elapsed();
    let leaves_sec = total_leaves as f64 / elapsed.as_secs_f64();
    println!(
        "\n  BENCH: {} files, {} leaves in {:.3}s",
        total_files,
        total_leaves,
        elapsed.as_secs_f64()
    );
    println!(
        "  Throughput: {:.0} leaves/s ({:.1}K leaves/s)",
        leaves_sec,
        leaves_sec / 1000.0
    );
}
