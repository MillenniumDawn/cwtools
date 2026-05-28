use clap::{Parser, Subcommand};
use cwtools_file_manager::file_manager::{FileManager, FileManagerConfig};
use cwtools_parser::parser::parse_string;
use cwtools_rules::rules_converter::ast_to_ruleset;
use cwtools_string_table::string_table::StringTable;
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
    /// Serialize AST to cache file (.cwb)
    Serialize {
        /// Input script file
        input: PathBuf,
        /// Output cache file
        output: PathBuf,
    },
    /// Deserialize cache file (.cwb) and verify
    Deserialize {
        /// Input cache file
        input: PathBuf,
    },
    /// Parse a .cwt rules file and print summary
    Rules {
        /// Path to the .cwt file
        file: PathBuf,
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
        Commands::Serialize { input, output } => {
            let input_str = std::fs::read_to_string(&input).unwrap_or_else(|e| {
                eprintln!("Error reading {}: {}", input.display(), e);
                std::process::exit(1);
            });
            let table = StringTable::new();
            match parse_string(&input_str, &table) {
                Ok(parsed) => {
                    let cached = cwtools_cache::convert::arena_to_cached(
                        &parsed.arena, &parsed.root_children, &table,
                    );
                    match cwtools_cache::io::serialize_to_file(&cached, &output) {
                        Ok(_) => {
                            println!("Serialized to {}", output.display());
                        }
                        Err(e) => {
                            eprintln!("Error serializing: {}", e);
                            std::process::exit(1);
                        }
                    }
                }
                Err(e) => {
                    eprintln!("Error parsing {}: {}", input.display(), e);
                    std::process::exit(1);
                }
            }
        }
        Commands::Deserialize { input } => {
            match cwtools_cache::io::deserialize_from_file(&input) {
                Ok(loaded) => {
                    let table = StringTable::new();
                    let (arena, root) = cwtools_cache::convert::cached_to_arena(&loaded, &table);
                    println!("Deserialized from {}", input.display());
                    println!("  Nodes:    {}", arena.nodes.len());
                    println!("  Leaves:   {}", arena.leaves.len());
                    println!("  Values:   {}", arena.leaf_values.len());
                    println!("  Clauses:  {}", arena.value_clauses.len());
                    println!("  Comments: {}", arena.comments.len());
                    println!("  Root children: {}", root.len());
                }
                Err(e) => {
                    eprintln!("Error deserializing {}: {}", input.display(), e);
                    std::process::exit(1);
                }
            }
        }
        Commands::Rules { file } => {
            let input_str = std::fs::read_to_string(&file).unwrap_or_else(|e| {
                eprintln!("Error reading {}: {}", file.display(), e);
                std::process::exit(1);
            });
            let table = StringTable::new();
            match parse_string(&input_str, &table) {
                Ok(parsed) => {
                    let ruleset = ast_to_ruleset(&parsed, &table);
                    println!("Parsed rules file: {}", file.display());
                    println!("  Types:         {}", ruleset.types.len());
                    for t in &ruleset.types {
                        println!("    - {} (path: {:?}, subtypes: {})", t.name, t.path_options.paths, t.subtypes.len());
                    }
                    println!("  Enums:         {}", ruleset.enums.len());
                    for e in &ruleset.enums {
                        println!("    - {} ({} values)", e.key, e.values.len());
                    }
                    println!("  Aliases:       {}", ruleset.aliases.len());
                    println!("  SingleAliases: {}", ruleset.single_aliases.len());
                    println!("  ComplexEnums:  {}", ruleset.complex_enums.len());
                }
                Err(e) => {
                    eprintln!("Error parsing {}: {}", file.display(), e);
                    std::process::exit(1);
                }
            }
        }
    }
}
