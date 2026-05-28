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
    /// Validate a directory of game files against .cwt rules
    Validate {
        /// Game identifier (hoi4, stellaris, eu4, ck2, ck3, vic2, vic3, ir, eu5, custom)
        #[arg(long, short)]
        game: String,
        /// Directory containing game files
        #[arg(long, short)]
        directory: PathBuf,
        /// Path to the .cwt rules file
        #[arg(long, short)]
        rules: PathBuf,
    },
}

/// Decide whether to search a directory directly (as a leaf directory containing .txt files)
/// or as a mod root with standard subfolders.
fn search_config_for(directory: &std::path::Path) -> FileManagerConfig {
    let known_script_folders = ["common", "events", "history", "interface", "decisions", "missions", "gfx"];
    let dir_name = directory.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");

    if known_script_folders.contains(&dir_name) || dir_name.ends_with(".txt") {
        // Search this directory directly
        FileManagerConfig {
            root: directory.to_path_buf(),
            include_dirs: vec![".".into()],
            ..Default::default()
        }
    } else {
        // Treat as mod root with standard subfolders
        FileManagerConfig {
            root: directory.to_path_buf(),
            ..Default::default()
        }
    }
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
            // If the directory is a specific subfolder (has .txt files), search directly.
            // Otherwise, treat it as a mod root and search standard subfolders.
            let config = search_config_for(&directory);
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
        Commands::Validate { game, directory, rules } => {
            use cwtools_game::constants::Game;
            use cwtools_validation::validate_ast;

            let game_id = Game::from_str(&game).unwrap_or_else(|| {
                eprintln!("Unknown game: {}. Supported: hoi4, stellaris, eu4, ck2, ck3, vic2, vic3, ir, eu5, custom", game);
                std::process::exit(1);
            });

            println!("Validating {} files in {} against {}", game_id, directory.display(), rules.display());

            // Parse rules
            let rules_str = std::fs::read_to_string(&rules).unwrap_or_else(|e| {
                eprintln!("Error reading rules {}: {}", rules.display(), e);
                std::process::exit(1);
            });
            let rules_table = StringTable::new();
            let rules_parsed = parse_string(&rules_str, &rules_table).unwrap_or_else(|e| {
                eprintln!("Error parsing rules {}: {}", rules.display(), e);
                std::process::exit(1);
            });
            let ruleset = ast_to_ruleset(&rules_parsed, &rules_table);
            println!("  Loaded {} types, {} enums", ruleset.types.len(), ruleset.enums.len());

            // Discover and parse files
            let config = search_config_for(&directory);
            let mut manager = FileManager::new(config);
            let files = manager.discover_and_parse().unwrap_or_else(|e| {
                eprintln!("Error discovering files: {}", e);
                std::process::exit(1);
            });
            println!("  Discovered {} files", files.len());

            // Validate each file
            let mut total_errors = 0;
            let mut total_warnings = 0;
            for file in files {
                let parser_file = cwtools_parser::ast::ParsedFile {
                    arena: file.arena,
                    root_children: file.root_children,
                };
                let errors = validate_ast(
                    &parser_file, &ruleset, &rules_table,
                );
                let file_errors: Vec<_> = errors.iter().filter(|e| e.severity == cwtools_validation::ErrorSeverity::Error).collect();
                let file_warnings: Vec<_> = errors.iter().filter(|e| e.severity == cwtools_validation::ErrorSeverity::Warning).collect();
                total_errors += file_errors.len();
                total_warnings += file_warnings.len();
                if !errors.is_empty() {
                    println!("\n  {}:", file.path.display());
                    for err in &errors {
                        println!("    [{:?}] {} (line {})", err.severity, err.message, err.line);
                    }
                }
            }

            println!("\nValidation complete: {} errors, {} warnings", total_errors, total_warnings);
            if total_errors > 0 {
                std::process::exit(1);
            }
        }
    }
}
