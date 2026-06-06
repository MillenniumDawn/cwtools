use clap::{Parser, Subcommand};
use cwtools_file_manager::file_manager::{FileManager, FileManagerConfig};
use cwtools_info::{
    TypeIndex, collect_set_variable_names, collect_type_instances, variable_defining_effects,
};
use cwtools_parser::parser::parse_string;
use cwtools_rules::rules_converter::ast_to_ruleset;
use cwtools_rules::rules_types::RuleSet;
use cwtools_rules::ruleset_loader::load_ruleset_from_dir;
use cwtools_string_table::string_table::StringTable;
use std::path::{Path, PathBuf};

use cwtools_info::vanilla_cache;

/// Build a TypeIndex from every script file under `dir` (used for a base-game
/// install). Files are parsed and indexed for reference resolution; they are
/// never validated.
fn index_game_dir(
    dir: &Path,
    ruleset: &RuleSet,
    table: &StringTable,
    var_effects: &std::collections::HashSet<String>,
) -> TypeIndex {
    let config = search_config_for(dir);
    let mut mgr = FileManager::with_string_table(config, table.clone());
    // `discover_and_parse` already parses the base-game files in parallel; the
    // expensive part. Collect type instances straight from those arenas (no
    // re-read/re-parse, unlike before) and stream-merge them sequentially so we
    // never hold every file's instances at once.
    let files = match mgr.discover_and_parse() {
        Ok(f) => f,
        Err(e) => {
            eprintln!(
                "  warn: could not read base-game dir {}: {}",
                dir.display(),
                e
            );
            return TypeIndex::new();
        }
    };
    eprintln!(
        "  Indexing {} base-game files from {}",
        files.len(),
        dir.display()
    );

    let mut index = TypeIndex::new();
    for file in files {
        let path = file.path.to_str().unwrap_or("").to_string();
        let pf = cwtools_parser::ast::ParsedFile {
            arena: file.arena,
            root_children: file.root_children,
            errors: vec![],
        };
        let instances = collect_type_instances(ruleset, &pf, &file.logical_path, table);
        index.merge(&path, instances);
        // Collect base-game variable definitions too, so a mod referencing a
        // vanilla variable isn't flagged as unset (CW246).
        if !var_effects.is_empty() {
            let mut names: Vec<String> = Vec::new();
            collect_set_variable_names(&pf, table, var_effects, &mut names);
            for n in &names {
                index.var_index.add_name(n);
            }
        }
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
        /// Report format: cli (default, grouped text), csv, or json.
        #[arg(long, default_value = "cli")]
        report_type: String,
        /// Write the report to this file instead of stdout.
        #[arg(long)]
        output_file: Option<PathBuf>,
        /// Suppress diagnostics whose hash is listed in this file (one hash per
        /// line). Lets you baseline known/accepted diagnostics and see only new ones.
        #[arg(long)]
        ignore_hashes: Option<PathBuf>,
        /// Write the surviving diagnostics' hashes (one per line) to this file, to
        /// use later with --ignore-hashes.
        #[arg(long)]
        output_hashes: Option<PathBuf>,
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
        "common",
        "events",
        "history",
        "interface",
        "decisions",
        "missions",
        "gfx",
        "sound",
        "music",
        "static_modifiers",
        "buildings",
        "technologies",
        "ethics",
        "policies",
        "ship_sizes",
        "pop_faction",
        "starbases_consolidated",
        "traits",
        "edicts",
        "traditions",
        "ascension_perks",
        "governments",
        "country_types",
        "bypass",
        "dlc_list",
        "subject_types",
        "casus_belli",
        "war_goals",
        "bombardment_stances",
        "armies",
        "deposits",
        "planet_classes",
        "tile_blockers",
        "species_rights",
        "observation_station_missions",
        "star_classes",
        "ambient_objects",
        "name_lists",
        "notification_modifier",
        "component_tags",
        "event_chains",
        "personalities",
        "global_ship_designs",
        "graphical_cultures",
        "species_archetypes",
        "resources",
        "species_classes",
        "buildable_pops",
        "opinion_modifiers",
        "leader_class_enum",
        "asteroid_belt",
        "solar_system_initializers",
        "fallen_empires",
    ];
    let dir_name = directory.file_name().and_then(|n| n.to_str()).unwrap_or("");

    // If this directory itself contains script files, search it directly.
    let script_exts = ["txt", "gui", "gfx", "sfx", "asset", "map"];
    let has_script_files = std::fs::read_dir(directory)
        .ok()
        .is_some_and(|mut entries| {
            entries.any(|e| {
                if let Ok(entry) = e {
                    entry
                        .path()
                        .extension()
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

/// Stable FNV-1a-64 hex digest of a diagnostic, for baseline/ignore matching.
/// Stable across runs and machines (unlike std's DefaultHasher seed).
fn diag_hash(file: &str, code: &str, message: &str, line: u32) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in file
        .bytes()
        .chain(b"|".iter().copied())
        .chain(code.bytes())
        .chain(b"|".iter().copied())
        .chain(message.bytes())
        .chain(b"|".iter().copied())
        .chain(line.to_string().bytes())
    {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{:016x}", h)
}

/// Escape a field for CSV output.
fn csv_escape(s: &str) -> String {
    if s.contains([',', '"', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// Minimal JSON string escape.
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
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
        eprintln!(
            "warn: CWTOOLS_FSHARP_CLI is set but not a file: {}",
            pb.display()
        );
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
        Commands::Validate {
            game,
            directory,
            rules,
            ..
        } => {
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
                    eprintln!(
                        "F# engine: failed to launch `dotnet`: {e}. Is the .NET runtime installed and on PATH?"
                    );
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

/// Initialize tracing only when RUST_LOG is set, so the default run stays quiet.
/// Profile a run with e.g. `RUST_LOG=info cwtools validate ...` and add
/// `#[tracing::instrument]` to any hot path you want timed. See PROFILING.md.
fn tracing_init() {
    if std::env::var("RUST_LOG").is_ok() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_target(true)
            // Emit span close events so instrumented hot paths report their
            // busy/idle time — that's the profiling signal.
            .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
            .try_init();
    }
}

fn main() {
    tracing_init();
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
                    println!(
                        "    - {} (path: {:?}, subtypes: {})",
                        t.name,
                        t.path_options.paths,
                        t.subtypes.len()
                    );
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
                    println!(
                        "Discovered and parsed {} files in {}",
                        files.len(),
                        directory.display()
                    );
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
                        &parsed.arena,
                        &parsed.root_children,
                        &table,
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
        Commands::Deserialize { input } => match cwtools_cache::io::deserialize_from_file(&input) {
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
        },
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
                println!(
                    "    - {} (path: {:?}, subtypes: {})",
                    t.name,
                    t.path_options.paths,
                    t.subtypes.len()
                );
            }
            println!("  Enums:         {}", ruleset.enums.len());
            for e in &ruleset.enums {
                println!("    - {} ({} values)", e.key, e.values.len());
            }
            println!("  Aliases:       {}", ruleset.aliases.len());
            println!("  SingleAliases: {}", ruleset.single_aliases.len());
            println!("  ComplexEnums:  {}", ruleset.complex_enums.len());
        }
        Commands::Validate {
            game,
            directory,
            rules,
            vanilla,
            vanilla_cache,
            report_type,
            output_file,
            ignore_hashes,
            output_hashes,
        } => {
            use cwtools_game::constants::Game;
            use cwtools_validation::validate_ast_with_loc;

            let game_id = Game::from_str(&game).unwrap_or_else(|| {
                eprintln!("Unknown game: {}. Supported: hoi4, stellaris, eu4, ck2, ck3, vic2, vic3, ir, eu5, custom", game);
                std::process::exit(1);
            });

            let rules_label = if rules.is_dir() {
                format!("directory {}", rules.display())
            } else {
                format!("file {}", rules.display())
            };
            eprintln!(
                "Validating {} files in {} against rules {}",
                game_id,
                directory.display(),
                rules_label
            );

            // Parse rules (shares its StringTable with game files)
            let rules_table = StringTable::new();
            let ruleset = load_rules(&rules, &rules_table);
            eprintln!(
                "  Loaded {} types, {} enums, {} aliases",
                ruleset.types.len(),
                ruleset.enums.len(),
                ruleset.aliases.len()
            );
            // Per-phase timings on stderr when CWTOOLS_TIMINGS is set.
            let _timings = std::env::var_os("CWTOOLS_TIMINGS").is_some();
            let mut _tprev = std::time::Instant::now();
            macro_rules! tlog {
                ($label:expr) => {{
                    if _timings {
                        eprintln!("  [t] {} {:?}", $label, _tprev.elapsed());
                    }
                    _tprev = std::time::Instant::now();
                }};
            }

            // Discover and parse files using the SAME string table
            let config = search_config_for(&directory);
            let mut manager = FileManager::with_string_table(config, rules_table.clone());
            let files = manager.discover_and_parse().unwrap_or_else(|e| {
                eprintln!("Error discovering files: {}", e);
                std::process::exit(1);
            });
            eprintln!("  Discovered {} files", files.len());
            tlog!("discover+parse");

            // Take ownership of each parsed AST once, as the parser's ParsedFile.
            // The TypeIndex build and the validation pass both borrow this set, so
            // nothing is parsed (or held) twice.
            let parsed: Vec<(std::path::PathBuf, String, cwtools_parser::ast::ParsedFile)> = files
                .into_iter()
                .map(|f| {
                    let pf = cwtools_parser::ast::ParsedFile {
                        arena: f.arena,
                        root_children: f.root_children,
                        errors: vec![],
                    };
                    (f.path, f.logical_path, pf)
                })
                .collect();

            // Build cross-file TypeIndex from the already-parsed arenas (Item 2).
            // This is cheap (~0.1s on MD), so keep it sequential and streaming:
            // merge each file's instances then drop them, rather than holding
            // every file's instances at once. Lower peak memory, and first-seen
            // dedup stays in deterministic input order.
            use rayon::prelude::*;
            let mut type_index = TypeIndex::new();
            for (path, logical_path, pf) in &parsed {
                let instances = collect_type_instances(&ruleset, pf, logical_path, &rules_table);
                type_index.merge(path.to_str().unwrap_or(""), instances);
            }
            tlog!("typeindex");

            // Project-wide variable index for `variable_field` reference checks
            // (CW246). Collect every variable defined by a `set_variable`-family
            // effect across the mod (the effect set is config-derived). Used only
            // when the var checks are enabled (CWTOOLS_VAR_CHECKS); building it is
            // cheap so it is always populated.
            let var_effects = variable_defining_effects(&ruleset);
            for (_path, _logical_path, pf) in &parsed {
                let mut names: Vec<String> = Vec::new();
                collect_set_variable_names(pf, &rules_table, &var_effects, &mut names);
                for n in &names {
                    type_index.var_index.add_name(n);
                }
            }
            tlog!("varindex");

            // Index the base-game install, if given. Vanilla files populate the
            // type index (so a mod can reference base-game operation_tokens,
            // ship_names, focuses, … without "not a known instance" errors) but are
            // never validated themselves.
            if let Some(vanilla_dir) = &vanilla {
                let vanilla_index =
                    index_game_dir(vanilla_dir, &ruleset, &rules_table, &var_effects);
                type_index.var_index.merge(&vanilla_index.var_index);
                for (type_name, entries) in vanilla_index.map {
                    let per_type = std::collections::HashMap::from([(
                        type_name,
                        entries.into_iter().map(|(_, inst)| inst).collect(),
                    )]);
                    type_index.merge("<vanilla>", per_type);
                }
                // Build the file index (mod + vanilla) for `filepath` reference
                // checks (CW113). Only when vanilla is present: mod files commonly
                // reference base-game assets, so an index missing vanilla would
                // flag every such reference as not-found.
                type_index.file_index.add_root(&directory);
                type_index.file_index.add_root(vanilla_dir);
                tlog!("fileindex");
            }

            // Load a pre-generated vanilla index, if given (faster than --vanilla;
            // resolves base-game references without re-parsing the install).
            if let Some(cache_path) = &vanilla_cache {
                match vanilla_cache::load(cache_path) {
                    Ok((cache_game, per_type)) => {
                        if cache_game != game {
                            eprintln!(
                                "  warn: vanilla cache was built for game '{}', validating '{}'",
                                cache_game, game
                            );
                        }
                        let total: usize = per_type.values().map(|v| v.len()).sum();
                        type_index.merge("<vanilla-cache>", per_type);
                        eprintln!(
                            "  Loaded {} base-game instances from cache {}",
                            total,
                            cache_path.display()
                        );
                    }
                    Err(e) => eprintln!(
                        "  warn: could not load vanilla cache {}: {}",
                        cache_path.display(),
                        e
                    ),
                }
            }

            // Modifier names valid in `alias_name[modifier]` slots (from the
            // top-level `modifiers = { ... }` block in the rules). Templated
            // entries like `production_speed_<building>_factor` /
            // `local_resources_<resource>_factor` / `<ideology>_drift` are
            // expanded against the type index, one per instance.
            let mut modifier_keys: std::collections::HashSet<String> =
                std::collections::HashSet::new();
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

            // Load localisation: the mod directory plus the vanilla install (so
            // mod config referencing base-game loc keys doesn't false-positive).
            // The combined service feeds the loc-key index (CW100/CW122) and the
            // loc-file checks; only mod-path loc files are reported.
            let mut loc_dirs: Vec<&std::path::Path> = vec![directory.as_path()];
            if let Some(v) = &vanilla {
                loc_dirs.push(v.as_path());
            }
            let loc_service = cwtools_localization::LocService::from_folders(&loc_dirs);
            let loc_game = match game_id {
                Game::Hoi4 => cwtools_localization::Game::HOI4,
                Game::Stellaris => cwtools_localization::Game::Stellaris,
                Game::Eu4 => cwtools_localization::Game::EU4,
                Game::Ck3 => cwtools_localization::Game::CK3,
                Game::Ir => cwtools_localization::Game::IR,
                Game::Vic3 => cwtools_localization::Game::VIC3,
                Game::Eu5 => cwtools_localization::Game::EU5,
                _ => cwtools_localization::Game::Generic,
            };
            tlog!("vanilla+modifiers");
            let loc_index = cwtools_localization::LocIndex::build(&loc_service, loc_game);
            tlog!("loc-load");

            // Load the ignore-hash baseline, if given.
            let ignored: std::collections::HashSet<String> = ignore_hashes
                .as_ref()
                .and_then(|p| std::fs::read_to_string(p).ok())
                .map(|s| {
                    s.lines()
                        .map(|l| l.trim().to_string())
                        .filter(|l| !l.is_empty())
                        .collect()
                })
                .unwrap_or_default();

            // Validate each file, collecting all (file, error, hash) diagnostics.
            struct Diag {
                file: String,
                severity: cwtools_validation::ErrorSeverity,
                code: String,
                message: String,
                line: u32,
                hash: String,
            }
            // Validate files in parallel. Each file's validation reads only
            // shared, immutable state (ruleset, rules_table behind its own lock,
            // type_index, modifier_keys, loc_index) and produces its own Vec, so
            // there's no contention on `diags`. `par_iter` over the indexed
            // `parsed` Vec collects in input order, so the report is byte-for-byte
            // identical to the sequential version.
            let ignored_ref = &ignored;
            let mut diags: Vec<Diag> = parsed
                .par_iter()
                .flat_map_iter(|(path, _logical_path, parser_file)| {
                    let file_str = path.to_str().unwrap_or("").to_string();
                    let errors = validate_ast_with_loc(
                        parser_file,
                        &ruleset,
                        &rules_table,
                        &file_str,
                        Some(game_id),
                        Some(&type_index),
                        Some(&modifier_keys),
                        Some(&loc_index),
                    );
                    errors.into_iter().filter_map(move |err| {
                        let code = err.code.clone().unwrap_or_default();
                        let hash = diag_hash(&file_str, &code, &err.message, err.line);
                        if ignored_ref.contains(&hash) {
                            return None;
                        }
                        Some(Diag {
                            file: file_str.clone(),
                            severity: err.severity,
                            code,
                            message: err.message,
                            line: err.line,
                            hash,
                        })
                    })
                })
                .collect();
            tlog!("validate-config");

            // Loc-file checks (CW225/CW234/CW259/CW268/CW275). Resolve refs
            // against the full mod+vanilla union but only report mod-path files.
            let dir_prefix = directory.to_string_lossy().to_string();
            for d in cwtools_localization::validate_loc_project(&loc_service, loc_game) {
                if !d.file.starts_with(&dir_prefix) {
                    continue;
                }
                let severity = match d.severity {
                    cwtools_localization::LocSeverity::Error => {
                        cwtools_validation::ErrorSeverity::Error
                    }
                    cwtools_localization::LocSeverity::Warning => {
                        cwtools_validation::ErrorSeverity::Warning
                    }
                    cwtools_localization::LocSeverity::Information => {
                        cwtools_validation::ErrorSeverity::Information
                    }
                };
                let line = d.line as u32;
                let code = d.code.to_string();
                let hash = diag_hash(&d.file, &code, &d.message, line);
                if ignored.contains(&hash) {
                    continue;
                }
                diags.push(Diag {
                    file: d.file,
                    severity,
                    code,
                    message: d.message,
                    line,
                    hash,
                });
            }
            tlog!("validate-loc");

            let total_errors = diags
                .iter()
                .filter(|d| d.severity == cwtools_validation::ErrorSeverity::Error)
                .count();
            let total_warnings = diags
                .iter()
                .filter(|d| d.severity == cwtools_validation::ErrorSeverity::Warning)
                .count();

            // Render the report in the requested format.
            let mut out = String::new();
            match report_type.as_str() {
                "csv" => {
                    out.push_str("file,line,severity,code,message,hash\n");
                    for d in &diags {
                        out.push_str(&format!(
                            "{},{},{:?},{},{},{}\n",
                            csv_escape(&d.file),
                            d.line,
                            d.severity,
                            csv_escape(&d.code),
                            csv_escape(&d.message),
                            d.hash
                        ));
                    }
                }
                "json" => {
                    out.push_str("[\n");
                    for (i, d) in diags.iter().enumerate() {
                        out.push_str(&format!(
                            "  {{\"file\":\"{}\",\"line\":{},\"severity\":\"{:?}\",\"code\":\"{}\",\"message\":\"{}\",\"hash\":\"{}\"}}{}\n",
                            json_escape(&d.file), d.line, d.severity, json_escape(&d.code), json_escape(&d.message), d.hash,
                            if i + 1 < diags.len() { "," } else { "" }));
                    }
                    out.push_str("]\n");
                }
                _ => {
                    // cli: grouped by file
                    let mut current = "";
                    for d in &diags {
                        if d.file != current {
                            out.push_str(&format!("\n  {}:\n", d.file));
                            current = &d.file;
                        }
                        let code_part = if d.code.is_empty() {
                            String::new()
                        } else {
                            format!("[{}] ", d.code)
                        };
                        out.push_str(&format!(
                            "    [{:?}] {}{} (line {})\n",
                            d.severity, code_part, d.message, d.line
                        ));
                    }
                    out.push_str(&format!(
                        "\nValidation complete: {} errors, {} warnings\n",
                        total_errors, total_warnings
                    ));
                }
            }

            match &output_file {
                Some(p) => {
                    if let Err(e) = std::fs::write(p, &out) {
                        eprintln!("Error writing report {}: {}", p.display(), e);
                    } else {
                        println!(
                            "Wrote {} report ({} errors, {} warnings) to {}",
                            report_type,
                            total_errors,
                            total_warnings,
                            p.display()
                        );
                    }
                }
                None => print!("{}", out),
            }

            // Write the surviving hashes for use as a future baseline.
            if let Some(p) = &output_hashes {
                let mut hashes: Vec<&str> = diags.iter().map(|d| d.hash.as_str()).collect();
                hashes.sort_unstable();
                hashes.dedup();
                if let Err(e) = std::fs::write(p, hashes.join("\n")) {
                    eprintln!("Error writing hashes {}: {}", p.display(), e);
                } else {
                    println!(
                        "Wrote {} diagnostic hashes to {}",
                        hashes.len(),
                        p.display()
                    );
                }
            }

            if total_errors > 0 {
                std::process::exit(1);
            }
        }
        Commands::CacheVanilla {
            game,
            vanilla,
            rules,
            output,
        } => {
            use cwtools_game::constants::Game;

            if Game::from_str(&game).is_none() {
                eprintln!(
                    "Unknown game: {}. Supported: hoi4, stellaris, eu4, ck2, ck3, vic2, vic3, ir, eu5, custom",
                    game
                );
                std::process::exit(1);
            }

            let rules_table = StringTable::new();
            let ruleset = load_rules(&rules, &rules_table);
            println!("  Loaded {} types from rules", ruleset.types.len());

            // The vanilla cache stores type instances only; no variable
            // collection needed here (empty effect set skips it).
            let index = index_game_dir(
                &vanilla,
                &ruleset,
                &rules_table,
                &std::collections::HashSet::new(),
            );
            match vanilla_cache::save(&index, &game, &output) {
                Ok(n) => println!("Wrote {} base-game instances to {}", n, output.display()),
                Err(e) => {
                    eprintln!("Error writing vanilla cache {}: {}", output.display(), e);
                    std::process::exit(1);
                }
            }
        }
        Commands::Loc { directory } => {
            use cwtools_localization::{LocService, validate_loc_project};

            println!("Scanning localisation in {}", directory.display());
            let service = LocService::from_folder(&directory);

            let total_entries: usize = service.files().iter().map(|f| f.entries.len()).sum();

            // Standalone loc lint uses the scope-independent checks (CW225 etc.);
            // scope-aware command checks need the referencing config's scope.
            let diags = validate_loc_project(&service, cwtools_localization::Game::Generic);

            // Surface parse failures too.
            let parse_errors: Vec<(String, String)> = service.errors().to_vec();

            let mut by_file: std::collections::BTreeMap<String, Vec<_>> =
                std::collections::BTreeMap::new();
            for d in &diags {
                by_file.entry(d.file.clone()).or_default().push(d);
            }
            for (file, ds) in &by_file {
                println!("\n  {} — {} issues:", file, ds.len());
                for d in ds {
                    println!("    [line {}] {}: {}", d.line, d.code, d.message);
                }
            }
            for (p, e) in &parse_errors {
                println!("\n  {} — PARSE ERROR: {}", p, e);
            }

            let total_issues = diags.len() + parse_errors.len();
            println!(
                "\nLoc validation complete: {} entries, {} issues",
                total_entries, total_issues
            );
            if total_issues > 0 {
                std::process::exit(1);
            }
        }
    }
}
