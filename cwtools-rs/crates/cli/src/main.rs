use clap::{Parser, Subcommand};
use cwtools_file_manager::file_manager::{FileManager, FileManagerConfig};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "cwtools")]
#[command(about = "CWTools CLI — Paradox mod tooling")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Parse a single Paradox script file and print AST summary
    Parse {
        /// Path to the file to parse
        file: PathBuf,
    },
    /// Discover and parse all files under a directory
    Discover {
        /// Root directory to search
        directory: PathBuf,
    },
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Parse { file } => {
            let mut manager = FileManager::new(FileManagerConfig::default());
            match manager.parse_single_file(&file) {
                Ok(parsed) => {
                    println!("Parsed: {}", file.display());
                    println!("  Nodes:     {}", parsed.arena.nodes.len());
                    println!("  Leaves:    {}", parsed.arena.leaves.len());
                    println!("  Values:    {}", parsed.arena.leaf_values.len());
                    println!("  Clauses:   {}", parsed.arena.value_clauses.len());
                    println!("  Comments:  {}", parsed.arena.comments.len());
                    println!("  Root children: {}", parsed.root_children.len());
                }
                Err(e) => {
                    eprintln!("Error parsing {}: {}", file.display(), e);
                    std::process::exit(1);
                }
            }
        }
        Commands::Discover { directory } => {
            let config = FileManagerConfig {
                root: directory.clone(),
                ..Default::default()
            };
            let mut manager = FileManager::new(config);
            match manager.discover_and_parse() {
                Ok(files) => {
                    println!("Discovered and parsed {} files in {}", files.len(), directory.display());
                    for f in files {
                        println!(
                            "  {} — nodes: {}, leaves: {}",
                            f.path.display(),
                            f.arena.nodes.len(),
                            f.arena.leaves.len()
                        );
                    }
                }
                Err(e) => {
                    eprintln!("Error discovering files in {}: {}", directory.display(), e);
                    std::process::exit(1);
                }
            }
        }
    }
}
