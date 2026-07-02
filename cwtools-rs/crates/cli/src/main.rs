use clap::{Parser, Subcommand};
use cwtools_driver::{index_game_dir, search_config_for};
use cwtools_file_manager::file_manager::{FileManager, FileManagerConfig};
use cwtools_parser::parser::parse_string;
use cwtools_rules::rules_types::RuleSet;
use cwtools_rules::ruleset_loader::load_ruleset_from_dir;
use cwtools_string_table::string_table::StringTable;
use std::borrow::Cow;
use std::path::PathBuf;

use cwtools_info::vanilla_cache;

#[derive(Parser)]
#[command(name = "cwtools")]
#[command(about = "CWTools CLI — Paradox mod tooling")]
struct Cli {
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
        /// Extra filename glob patterns to skip (in addition to the engine
        /// defaults like Changelog.txt, README.md, *.md). May be repeated.
        /// Examples: --ignore-file "secret*" --ignore-file "*.notes"
        #[arg(long = "ignore-file", value_name = "GLOB")]
        ignore_files: Vec<String>,
        /// Extra directory glob patterns to skip during workspace discovery.
        /// May be repeated. Examples: --ignore-dir "build" --ignore-dir "temp*"
        #[arg(long = "ignore-dir", value_name = "GLOB")]
        ignore_dirs: Vec<String>,
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
fn csv_escape(s: &str) -> Cow<'_, str> {
    if s.contains([',', '"', '\n']) {
        Cow::Owned(format!("\"{}\"", s.replace('"', "\"\"")))
    } else {
        Cow::Borrowed(s)
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

/// Load a RuleSet from either a single `.cwt` file or a directory of `.cwt`
/// files (shared loader in the driver). Rules-load failure is fatal in the CLI.
fn load_rules(rules_path: &std::path::Path, table: &StringTable) -> RuleSet {
    let mut warn = |w: String| eprintln!("warn: {}", w);
    cwtools_driver::load_rules(
        &cwtools_driver::RulesInput::from_path(rules_path.to_path_buf()),
        table,
        Some(&mut warn),
    )
    .unwrap_or_else(|e| {
        eprintln!("Error loading rules: {}", e);
        std::process::exit(1);
    })
}

/// Print a compact summary of a loaded RuleSet. Shared by the Parse-on-directory
/// and Rules subcommands (previously copy-pasted between them).
fn print_ruleset_summary(ruleset: &cwtools_rules::rules_types::RuleSet) {
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

/// Map a run's outcome to a process exit code. Operational failures (couldn't
/// discover the files, couldn't write the report) are distinct from validation
/// finding errors, so CI can tell "the tool couldn't run" apart from "validation
/// found problems". 0 = clean run, no errors.
fn exit_code(total_errors: usize, discovery_failed: bool, write_failed: bool) -> i32 {
    if discovery_failed {
        3
    } else if write_failed {
        2
    } else if total_errors > 0 {
        1
    } else {
        0
    }
}

fn main() {
    // Quiet by default; set RUST_LOG or CWTOOLS_PROFILE to turn on logging /
    // profiling. See PROFILING.md and `cwtools_profiling`.
    cwtools_profiling::init_tracing();
    let cli = Cli::parse();

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
                print_ruleset_summary(&ruleset);
            } else {
                let mut manager = FileManager::new(FileManagerConfig::default());
                match manager.parse_single_file(&file) {
                    Ok(parsed) => {
                        println!("Parsed: {}", file.display());
                        println!("  Logical path:  {}", parsed.logical_path);
                        println!("  Leaves:        {}", parsed.arena.leaves.len());
                        println!("  Values:        {}", parsed.arena.leaf_values.len());
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
                            "  {} [{}] — leaves: {}",
                            f.logical_path,
                            f.path.display(),
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
        Commands::Deserialize { input } => {
            let table = StringTable::new();
            let result = cwtools_cache::io::with_archived_file(&input, |archived| {
                cwtools_cache::convert::archived_to_arena(archived, &table)
            });
            match result {
                Ok((arena, root)) => {
                    println!("Deserialized from {}", input.display());
                    println!("  Leaves:   {}", arena.leaves.len());
                    println!("  Values:   {}", arena.leaf_values.len());
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
            print_ruleset_summary(&ruleset);
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
            ignore_files,
            ignore_dirs,
        } => {
            use cwtools_driver::{RulesInput, Session, SessionConfig};
            use cwtools_game::constants::Game;

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

            // Load a pre-generated vanilla index, if given (faster than --vanilla;
            // resolves base-game references without re-parsing the install).
            // Fingerprint comparison happens after the session is loaded (needs
            // the ruleset); stale caches are detected there and re-generated.
            let vanilla_cache_index = vanilla_cache.as_ref().and_then(|cache_path| {
                match vanilla_cache::load(cache_path) {
                    Ok((cache_game, cached_fp, data)) => {
                        if cache_game != game {
                            eprintln!(
                                "  warn: vanilla cache was built for game '{}', validating '{}'",
                                cache_game, game
                            );
                        }
                        let total: usize = data.per_type.values().map(|v| v.len()).sum();
                        eprintln!(
                            "  Loaded {} base-game instances, {} loc languages, {} files from cache {} (fp: {})",
                            total,
                            data.loc_keys.len(),
                            data.file_paths.len(),
                            cache_path.display(),
                            cached_fp,
                        );
                        Some((cached_fp, data))
                    }
                    Err(e) => {
                        eprintln!(
                            "  warn: could not load vanilla cache {}: {}",
                            cache_path.display(),
                            e
                        );
                        None
                    }
                }
            });
            let (cached_fingerprint, vanilla_cache_index) = vanilla_cache_index.unzip();

            // Build the whole engine pipeline through the shared driver: parse
            // rules, discover/parse mod files, build the type/var/vanilla indexes,
            // expand modifier keys, build the loc index, prebuild the scope
            // registry. The CLI and LSP share this one implementation.
            let session = Session::load(SessionConfig {
                game: game_id,
                rules: RulesInput::from_path(rules.clone()),
                directory: directory.clone(),
                vanilla: vanilla.clone(),
                vanilla_cache: vanilla_cache_index,
                ignore_files: &ignore_files,
                ignore_dirs: &ignore_dirs,
                loc_languages: None,
                on_rules_warning: Some(&mut |w: String| eprintln!("warn: {}", w)),
            });
            let ruleset = session.ruleset();
            eprintln!(
                "  Loaded {} types, {} enums, {} aliases",
                ruleset.types.len(),
                ruleset.enums.len(),
                ruleset.aliases.len()
            );
            eprintln!("  Discovered {} files", session.parsed_files().len());

            // Vanilla-cache freshness check. If both --vanilla-cache and --vanilla
            // are given we can compute the combined fingerprint (game version +
            // ruleset shape) and detect staleness. THIS run already used the
            // cached data (the cache short-circuits the vanilla walk); the
            // rebuild makes the next run correct.
            if let (Some(cache_path), Some(fp_loaded), Some(vanilla_dir)) =
                (&vanilla_cache, &cached_fingerprint, &vanilla)
            {
                let fp_live = vanilla_cache::combined_fingerprint(vanilla_dir, ruleset);
                if *fp_loaded != fp_live {
                    eprintln!(
                        "  warn: vanilla cache is stale (cached: {}, live: {}); rebuilding",
                        fp_loaded, fp_live
                    );
                    let rules_table = session.string_table();
                    let var_effects = cwtools_info::variable_defining_effects(ruleset);
                    let index = index_game_dir(vanilla_dir, ruleset, rules_table, &var_effects);
                    let aux = cwtools_driver::build_vanilla_cache_aux(vanilla_dir, &index);
                    match vanilla_cache::save(&index, &game, &fp_live, cache_path, aux) {
                        Ok(n) => eprintln!("  Rebuilt vanilla cache with {} instances", n),
                        Err(e) => eprintln!(
                            "  warn: could not write rebuilt cache {}: {}",
                            cache_path.display(),
                            e
                        ),
                    }
                }
            }

            tlog!("load");

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
            // The driver validates files in parallel, in input order, so the
            // report is byte-for-byte identical to the sequential version.
            let ignored_ref = &ignored;
            let mut diags: Vec<Diag> = session
                .validate_all()
                .into_iter()
                .flat_map(|(path, errors)| {
                    let file_str = path.to_str().unwrap_or("").to_string();
                    errors.into_iter().filter_map(move |err| {
                        let code = err.code.unwrap_or_default().to_string();
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
            // Ensure the prefix has a trailing separator so `/mods/MD` doesn't
            // accidentally match `/mods/MD-assets`.
            let dir_prefix = {
                let s = directory.to_string_lossy();
                if s.ends_with(std::path::MAIN_SEPARATOR) {
                    s.into_owned()
                } else {
                    format!("{}{}", s, std::path::MAIN_SEPARATOR)
                }
            };
            for d in session.loc_project_diagnostics() {
                if !d.file.starts_with(&dir_prefix) {
                    continue;
                }
                let severity = d.severity;
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

            // Memory report (CWTOOLS_PROFILE=1): RSS at the end of a single
            // validate pass (a good proxy for peak) plus a per-component
            // breakdown, to track the 1.5 GB target and see where bytes go.
            if cwtools_profiling::profile_enabled() {
                let mib = |b: usize| cwtools_profiling::format_mib(b as u64);
                let parsed = session.parsed_files();
                let type_index = session.type_index();
                let loc_index = session.loc_index();
                let rules_table = session.string_table();
                if let Some(rss) = cwtools_profiling::current_rss_bytes() {
                    eprintln!(
                        "  [profile] RSS {} after validating {} files",
                        cwtools_profiling::format_mib(rss),
                        parsed.len()
                    );
                }
                let st = rules_table.stats();
                eprintln!(
                    "  [profile]   string_table: {} ({} entries, strings {}, keys {})",
                    mib(st.total_bytes()),
                    st.entries,
                    mib(st.id_to_string_bytes),
                    mib(st.map_key_bytes),
                );
                let (mut leaves, mut values) = (0usize, 0);
                for src in parsed {
                    leaves += src.parsed.arena.leaves.len();
                    values += src.parsed.arena.leaf_values.len();
                }
                let type_instances: usize = type_index.map.values().map(|v| v.len()).sum();
                eprintln!(
                    "  [profile]   arenas: {} leaves, {} values across {} files",
                    leaves,
                    values,
                    parsed.len()
                );
                eprintln!(
                    "  [profile]   type_index: {} instances in {} types; loc union: {} keys",
                    type_instances,
                    type_index.map.len(),
                    loc_index.union().len()
                );
            }

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

            let write_failed = match &output_file {
                Some(p) => {
                    if let Err(e) = std::fs::write(p, &out) {
                        eprintln!("Error writing report {}: {}", p.display(), e);
                        true
                    } else {
                        println!(
                            "Wrote {} report ({} errors, {} warnings) to {}",
                            report_type,
                            total_errors,
                            total_warnings,
                            p.display()
                        );
                        false
                    }
                }
                None => {
                    print!("{}", out);
                    false
                }
            };

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

            let code = exit_code(total_errors, session.discovery_failed, write_failed);
            if code != 0 {
                std::process::exit(code);
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

            let var_effects = cwtools_info::variable_defining_effects(&ruleset);
            let index = index_game_dir(&vanilla, &ruleset, &rules_table, &var_effects);
            // Loc keys + file paths + variable names ride along so a cache hit
            // also skips the loc walk and file-index walk over the install.
            let aux = cwtools_driver::build_vanilla_cache_aux(&vanilla, &index);
            // Combined fingerprint = game version + ruleset shape, so a cache
            // built against one rules set is treated as stale by another (the
            // cached instances are extracted by the rules; a rules change can
            // change which instances exist and under what name).
            let fingerprint = vanilla_cache::combined_fingerprint(&vanilla, &ruleset);
            println!("  Vanilla fingerprint: {}", fingerprint);
            match vanilla_cache::save(&index, &game, &fingerprint, &output, aux) {
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
            let diags = validate_loc_project(&service);

            // Surface parse failures too.
            let parse_errors = service.errors();

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
            for (p, e) in parse_errors {
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

#[cfg(test)]
mod tests {
    use super::exit_code;

    #[test]
    fn exit_code_separates_operational_from_validation() {
        assert_eq!(exit_code(0, false, false), 0); // clean
        assert_eq!(exit_code(5, false, false), 1); // validation errors
        assert_eq!(exit_code(0, false, true), 2); // report write failed
        assert_eq!(exit_code(0, true, false), 3); // discovery failed
        // operational failures take precedence over validation errors
        assert_eq!(exit_code(5, false, true), 2);
        assert_eq!(exit_code(5, true, true), 3);
    }
}
