use clap::{Parser, Subcommand};
use cwtools_file_manager::file_manager::{FileManager, FileManagerConfig};
use cwtools_info::{collect_type_instances, TypeIndex};
use cwtools_parser::parser::parse_string;
use cwtools_rules::ruleset_loader::load_ruleset_from_dir;
use cwtools_rules::rules_converter::ast_to_ruleset;
use cwtools_rules::rules_types::RuleSet;
use cwtools_string_table::string_table::StringTable;
use std::path::{Path, PathBuf};

use cwtools_info::vanilla_cache;

/// Build a TypeIndex from every script file under `dir` (used for a base-game
/// install). Files are parsed and indexed for reference resolution; they are
/// never validated.
fn index_game_dir(dir: &Path, ruleset: &RuleSet, table: &StringTable) -> TypeIndex {
    let mut index = TypeIndex::new();
    let config = search_config_for(dir);
    let mut mgr = FileManager::with_string_table(config, table.clone());
    match mgr.discover_and_parse() {
        Ok(files) => {
            println!("  Indexing {} base-game files from {}", files.len(), dir.display());
            for file in &files {
                if let Ok(text) = std::fs::read_to_string(&file.path)
                    && let Ok(pf) = parse_string(&text, table)
                {
                    let instances = collect_type_instances(ruleset, &pf, &file.logical_path, table);
                    index.merge(file.path.to_str().unwrap_or(""), instances);
                }
            }
        }
        Err(e) => eprintln!("  warn: could not read base-game dir {}: {}", dir.display(), e),
    }
    index
}

#[derive(Parser)]
#[command(name = "cwtools")]
#[command(about = "CWTools CLI — Paradox mod tooling")]
struct Cli {
    /// Engine to run: "rust" (default, this binary) or "fsharp" (delegates to the
    /// original CWToolsCLI.dll via `dotnet`). The F# engine currently supports
    /// only the `validate` subcommand. Locate the dll via the CWTOOLS_FSHARP_CLI
    /// env var (path to CWToolsCLI.dll).
    #[arg(long, global = true, default_value = "rust")]
    engine: String,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Parse a single Paradox script file (or a directory of .cwt rule files) and print summary
    Parse {
        /// Path to a file or a directory of .cwt files
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
    /// Parse a .cwt rules file or directory and print summary
    Rules {
        /// Path to a .cwt file or a directory containing .cwt files
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
        /// Path to a .cwt rules file OR a directory containing .cwt rule files
        #[arg(long, short)]
        rules: PathBuf,
        /// Optional path to the base game install (e.g. the vanilla HOI4 folder).
        /// Its files are indexed for reference resolution but not validated, so a
        /// mod can reference base-game content (operation_tokens, ship_names, …)
        /// without false "not a known instance" errors.
        #[arg(long)]
        vanilla: Option<PathBuf>,
        /// Optional pre-generated vanilla index (see `cache-vanilla`). Loaded for
        /// reference resolution without re-parsing the game install. Faster than
        /// `--vanilla`; can be combined with it.
        #[arg(long)]
        vanilla_cache: Option<PathBuf>,
    },
    /// Pre-generate a vanilla type index from a base-game install, for use with
    /// `validate --vanilla-cache`. Parses and indexes the install once so later
    /// runs resolve base-game references without re-parsing it.
    CacheVanilla {
        /// Game identifier (hoi4, stellaris, eu4, ck2, ck3, vic2, vic3, ir, eu5, custom)
        #[arg(long, short)]
        game: String,
        /// Base-game install directory to index
        #[arg(long)]
        vanilla: PathBuf,
        /// Path to a .cwt rules file OR a directory containing .cwt rule files
        #[arg(long, short)]
        rules: PathBuf,
        /// Output cache file to write
        #[arg(long, short)]
        output: PathBuf,
    },
    /// Parse and validate localisation files (.yml)
    Loc {
        /// Directory containing localisation .yml files
        directory: PathBuf,
    },
}

/// Decide whether to search a directory directly (as a leaf directory containing .txt files)
/// or as a mod root with standard subfolders.
fn search_config_for(directory: &std::path::Path) -> FileManagerConfig {
    let known_script_folders = [
        "common", "events", "history", "interface", "decisions", "missions", "gfx",
        "static_modifiers", "buildings", "technologies", "ethics", "policies",
        "ship_sizes", "pop_faction", "starbases_consolidated", "traits", "edicts",
        "traditions", "ascension_perks", "governments", "country_types", "bypass",
        "dlc_list", "subject_types", "casus_belli", "war_goals", "bombardment_stances",
        "armies", "deposits", "planet_classes", "tile_blockers", "species_rights",
        "observation_station_missions", "star_classes", "ambient_objects", "name_lists",
        "notification_modifier", "component_tags", "event_chains", "personalities",
        "global_ship_designs", "graphical_cultures", "species_archetypes", "resources",
        "species_classes", "buildable_pops", "opinion_modifiers", "leader_class_enum",
        "asteroid_belt", "solar_system_initializers", "fallen_empires",
    ];
    let dir_name = directory.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");

    // If this directory itself contains script files, search it directly.
    let script_exts = ["txt", "gui", "gfx", "sfx", "asset", "map"];
    let has_script_files = std::fs::read_dir(directory).ok().is_some_and(|mut entries| {
        entries.any(|e| {
            if let Ok(entry) = e {
                entry.path().extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| script_exts.contains(&ext))
            } else {
                false
            }
        })
    });

    if known_script_folders.contains(&dir_name) || dir_name.ends_with(".txt") || has_script_files {
        FileManagerConfig {
            root: directory.to_path_buf(),
            include_dirs: vec![".".into()],
            ..Default::default()
        }
    } else {
        FileManagerConfig {
            root: directory.to_path_buf(),
            ..Default::default()
        }
    }
}

/// Load a RuleSet from either a single `.cwt` file or a directory of `.cwt` files.
fn load_rules(rules_path: &std::path::Path, table: &StringTable) -> RuleSet {
    if rules_path.is_dir() {
        let (ruleset, errors) = load_ruleset_from_dir(rules_path, table);
        for err in &errors {
            eprintln!("warn: {}", err);
        }
        ruleset
    } else {
        let rules_str = std::fs::read_to_string(rules_path).unwrap_or_else(|e| {
            eprintln!("Error reading rules {}: {}", rules_path.display(), e);
            std::process::exit(1);
        });
        let parsed = parse_string(&rules_str, table).unwrap_or_else(|e| {
            eprintln!("Error parsing rules {}: {}", rules_path.display(), e);
            std::process::exit(1);
        });
        ast_to_ruleset(&parsed, table)
    }
}

/// Locate the F# CWToolsCLI.dll: the CWTOOLS_FSHARP_CLI env var wins, otherwise
/// try a couple of conventional build-output paths relative to the cwd.
fn locate_fsharp_cli() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("CWTOOLS_FSHARP_CLI") {
        let pb = PathBuf::from(p);
        if pb.is_file() {
            return Some(pb);
        }
        eprintln!("warn: CWTOOLS_FSHARP_CLI is set but not a file: {}", pb.display());
    }
    for c in [
        "../artifacts/bin/CWToolsCLI/release/CWToolsCLI.dll",
        "../artifacts/bin/CWToolsCLI/debug/CWToolsCLI.dll",
        "artifacts/bin/CWToolsCLI/release/CWToolsCLI.dll",
    ] {
        let pb = PathBuf::from(c);
        if pb.is_file() {
            return Some(pb);
        }
    }
    None
}

/// Delegate to the original F# engine (CWToolsCLI.dll over `dotnet`). Only the
/// `validate` subcommand is supported; everything else is rust-only.
fn run_fsharp_engine(command: &Commands) -> ! {
    match command {
        Commands::Validate { game, directory, rules, .. } => {
            let dll = locate_fsharp_cli().unwrap_or_else(|| {
                eprintln!(
                    "F# engine: CWToolsCLI.dll not found. Set CWTOOLS_FSHARP_CLI to its path, \
                     or build it with `dotnet build CWToolsCLI/CWToolsCLI.fsproj -c Release`."
                );
                std::process::exit(1);
            });
            eprintln!("Delegating to F# engine: {}", dll.display());
            let status = std::process::Command::new("dotnet")
                .arg(&dll)
                .arg("--game")
                .arg(game)
                .arg("--directory")
                .arg(directory)
                .arg("--rulespath")
                .arg(rules)
                .arg("validate")
                .arg("all")
                .status();
            match status {
                Ok(s) => std::process::exit(s.code().unwrap_or(1)),
                Err(e) => {
                    eprintln!("F# engine: failed to launch `dotnet`: {e}. Is the .NET runtime installed and on PATH?");
                    std::process::exit(1);
                }
            }
        }
        _ => {
            eprintln!(
                "The F# engine (--engine fsharp) only supports the `validate` subcommand. \
                 Use --engine rust (the default) for other commands."
            );
            std::process::exit(2);
        }
    }
}

fn main() {
    let cli = Cli::parse();

    match cli.engine.as_str() {
        "rust" => {}
        "fsharp" => run_fsharp_engine(&cli.command),
        other => {
            eprintln!("Unknown engine '{other}'. Valid values: rust, fsharp.");
            std::process::exit(2);
        }
    }

    match cli.command {
        Commands::Parse { file } => {
            if file.is_dir() {
                // Treat as a directory of .cwt rule files
                let table = StringTable::new();
                let (ruleset, errors) = load_ruleset_from_dir(&file, &table);
                for err in &errors {
                    eprintln!("warn: {}", err);
                }
                println!("Parsed rule directory: {}", file.display());
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
            } else {
                let mut manager = FileManager::new(FileManagerConfig::default());
                match manager.parse_single_file(&file) {
                    Ok(parsed) => {
                        println!("Parsed: {}", file.display());
                        println!("  Logical path:  {}", parsed.logical_path);
                        println!("  Nodes:         {}", parsed.arena.nodes.len());
                        println!("  Leaves:        {}", parsed.arena.leaves.len());
                        println!("  Values:        {}", parsed.arena.leaf_values.len());
                        println!("  Clauses:       {}", parsed.arena.value_clauses.len());
                        println!("  Comments:      {}", parsed.arena.comments.len());
                        println!("  Root children: {}", parsed.root_children.len());
                    }
                    Err(e) => {
                        eprintln!("Error parsing {}: {}", file.display(), e);
                        std::process::exit(1);
                    }
                }
            }
        }
        Commands::Discover { directory } => {
            let config = search_config_for(&directory);
            let mut manager = FileManager::new(config);
            match manager.discover_and_parse() {
                Ok(files) => {
                    println!("Discovered and parsed {} files in {}", files.len(), directory.display());
                    for f in files {
                        println!(
                            "  {} [{}] — nodes: {}, leaves: {}",
                            f.logical_path,
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
            let table = StringTable::new();
            let ruleset = load_rules(&file, &table);
            let label = if file.is_dir() {
                format!("rule directory: {}", file.display())
            } else {
                format!("rules file: {}", file.display())
            };
            println!("Parsed {}", label);
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
        Commands::Validate { game, directory, rules, vanilla, vanilla_cache } => {
            use cwtools_game::constants::Game;
            use cwtools_validation::validate_ast;

            let game_id = Game::from_str(&game).unwrap_or_else(|| {
                eprintln!("Unknown game: {}. Supported: hoi4, stellaris, eu4, ck2, ck3, vic2, vic3, ir, eu5, custom", game);
                std::process::exit(1);
            });

            let rules_label = if rules.is_dir() {
                format!("directory {}", rules.display())
            } else {
                format!("file {}", rules.display())
            };
            println!("Validating {} files in {} against rules {}", game_id, directory.display(), rules_label);

            // Parse rules (shares its StringTable with game files)
            let rules_table = StringTable::new();
            let ruleset = load_rules(&rules, &rules_table);
            println!("  Loaded {} types, {} enums, {} aliases", ruleset.types.len(), ruleset.enums.len(), ruleset.aliases.len());

            // Discover and parse files using the SAME string table
            let config = search_config_for(&directory);
            let mut manager = FileManager::with_string_table(config, rules_table.clone());
            let files = manager.discover_and_parse().unwrap_or_else(|e| {
                eprintln!("Error discovering files: {}", e);
                std::process::exit(1);
            });
            println!("  Discovered {} files", files.len());

            // Build cross-file TypeIndex from all discovered files (Item 2).
            // Arena doesn't derive Clone, so we re-read each file to build the
            // index, then use the already-parsed arenas for validation.
            let mut type_index = TypeIndex::new();
            for file in &files {
                let text = match std::fs::read_to_string(&file.path) {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                if let Ok(pf) = cwtools_parser::parser::parse_string(&text, &rules_table) {
                    let instances = collect_type_instances(&ruleset, &pf, &file.logical_path, &rules_table);
                    type_index.merge(file.path.to_str().unwrap_or(""), instances);
                }
            }

            // Index the base-game install, if given. Vanilla files populate the
            // type index (so a mod can reference base-game operation_tokens,
            // ship_names, focuses, … without "not a known instance" errors) but are
            // never validated themselves.
            if let Some(vanilla_dir) = &vanilla {
                let vanilla_index = index_game_dir(vanilla_dir, &ruleset, &rules_table);
                for (type_name, entries) in vanilla_index.map {
                    let per_type = std::collections::HashMap::from([(
                        type_name,
                        entries.into_iter().map(|(_, inst)| inst).collect(),
                    )]);
                    type_index.merge("<vanilla>", per_type);
                }
            }

            // Load a pre-generated vanilla index, if given (faster than --vanilla;
            // resolves base-game references without re-parsing the install).
            if let Some(cache_path) = &vanilla_cache {
                match vanilla_cache::load(cache_path) {
                    Ok((cache_game, per_type)) => {
                        if cache_game != game {
                            eprintln!("  warn: vanilla cache was built for game '{}', validating '{}'", cache_game, game);
                        }
                        let total: usize = per_type.values().map(|v| v.len()).sum();
                        type_index.merge("<vanilla-cache>", per_type);
                        println!("  Loaded {} base-game instances from cache {}", total, cache_path.display());
                    }
                    Err(e) => eprintln!("  warn: could not load vanilla cache {}: {}", cache_path.display(), e),
                }
            }

            // Modifier names valid in `alias_name[modifier]` slots (from the
            // top-level `modifiers = { ... }` block in the rules). Templated
            // entries like `production_speed_<building>_factor` /
            // `local_resources_<resource>_factor` / `<ideology>_drift` are
            // expanded against the type index, one per instance.
            let mut modifier_keys: std::collections::HashSet<String> = std::collections::HashSet::new();
            for m in &ruleset.modifiers {
                match (m.find('<'), m.find('>')) {
                    (Some(open), Some(close)) if open < close => {
                        let tn = &m[open + 1..close];
                        let pre = &m[..open];
                        let suf = &m[close + 1..];
                        for (_uri, inst) in type_index.instances(tn) {
                            modifier_keys.insert(format!("{}{}{}", pre, inst.name, suf));
                        }
                    }
                    _ => {
                        modifier_keys.insert(m.clone());
                    }
                }
            }

            // Validate each file
            let mut total_errors = 0;
            let mut total_warnings = 0;
            for file in files {
                let parser_file = cwtools_parser::ast::ParsedFile {
                    arena: file.arena,
                    root_children: file.root_children,
                    errors: vec![],
                };
                let errors = validate_ast(
                    &parser_file, &ruleset, &rules_table, file.path.to_str().unwrap_or(""),
                    Some(game_id), Some(&type_index), Some(&modifier_keys),
                );
                let file_errors: Vec<_> = errors.iter().filter(|e| e.severity == cwtools_validation::ErrorSeverity::Error).collect();
                let file_warnings: Vec<_> = errors.iter().filter(|e| e.severity == cwtools_validation::ErrorSeverity::Warning).collect();
                total_errors += file_errors.len();
                total_warnings += file_warnings.len();
                if !errors.is_empty() {
                    println!("\n  {}:", file.path.display());
                    for err in &errors {
                        let code_part = err.code.as_deref().map(|c| format!("[{}] ", c)).unwrap_or_default();
                        println!("    [{:?}] {}{} (line {})", err.severity, code_part, err.message, err.line);
                    }
                }
            }

            println!("\nValidation complete: {} errors, {} warnings", total_errors, total_warnings);
            if total_errors > 0 {
                std::process::exit(1);
            }
        }
        Commands::CacheVanilla { game, vanilla, rules, output } => {
            use cwtools_game::constants::Game;

            if Game::from_str(&game).is_none() {
                eprintln!("Unknown game: {}. Supported: hoi4, stellaris, eu4, ck2, ck3, vic2, vic3, ir, eu5, custom", game);
                std::process::exit(1);
            }

            let rules_table = StringTable::new();
            let ruleset = load_rules(&rules, &rules_table);
            println!("  Loaded {} types from rules", ruleset.types.len());

            let index = index_game_dir(&vanilla, &ruleset, &rules_table);
            match vanilla_cache::save(&index, &game, &output) {
                Ok(n) => println!("Wrote {} base-game instances to {}", n, output.display()),
                Err(e) => {
                    eprintln!("Error writing vanilla cache {}: {}", output.display(), e);
                    std::process::exit(1);
                }
            }
        }
        Commands::Loc { directory } => {
            use cwtools_localization::service::LocService;
            use cwtools_localization::validation::validate_loc_file;
            use cwtools_localization::validation::build_key_union;

            println!("Scanning localisation in {}", directory.display());
            let service = LocService::from_folder(&directory);

            let mut all_files = Vec::new();
            for (_, result) in service.results() {
                if let Ok(file) = result {
                    all_files.push(file.clone());
                }
            }
            let all_keys = build_key_union(&all_files);
            println!("  Total unique keys: {}", all_keys.len());

            let hardcoded: Vec<&str> = vec![
                "Player", "Root", "From", "Prev", "Capital", "Random", "This",
                "Country", "Ruler", "GetName", "GetName2", "GetSpeciesName",
                "GetSpeciesNamePlural", "GetSpeciesAdj", "GetTitle",
                "Owner", "Controller", "GetGovernmentName", "GetClassName",
                "GetAdj", "GetIcon", "GetRegnalName", "Date", "GetDate",
            ];

            let mut total_errors = 0;
            let mut total_entries = 0;

            for (path, result) in service.results() {
                match result {
                    Ok(file) => {
                        let mut file_copy = file.clone();
                        let errors = validate_loc_file(
                            &mut file_copy, &all_keys, &hardcoded
                        );
                        total_entries += file.entries.len();
                        if !errors.is_empty() {
                            println!("\n  {} — {} errors:", path, errors.len());
                            for err in &errors {
                                println!("    [line {}] {}", err.line, err.message);
                            }
                            total_errors += errors.len();
                        }
                    }
                    Err(e) => {
                        println!("\n  {} — PARSE ERROR: {}", path, e);
                        total_errors += 1;
                    }
                }
            }

            println!("\nLoc validation complete: {} entries, {} errors", total_entries, total_errors);
            if total_errors > 0 {
                std::process::exit(1);
            }
        }
    }
}
