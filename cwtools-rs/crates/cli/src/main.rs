use clap::{Parser, Subcommand};
use cwtools_driver::{index_game_dir, search_config_for};
use cwtools_file_manager::file_manager::{FileManager, FileManagerConfig};
use cwtools_localization::Lang;
use cwtools_parser::parser::parse_string;
use cwtools_rules::rules_types::RuleSet;
use cwtools_rules::ruleset_loader::load_ruleset_from_dir;
use cwtools_string_table::string_table::StringTable;
use cwtools_validation::{ErrorSeverity, ValidationError};
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
        /// Restrict loc validation/lookup to this language (repeatable). Valid
        /// values: english, french, german, spanish, russian, polish, braz_por,
        /// simp_chinese, japanese, korean, turkish, default. Omit to use every
        /// language with data (current behavior).
        #[arg(long = "loc-language", value_name = "LANG", value_parser = parse_lang)]
        loc_language: Vec<Lang>,
        /// Only report diagnostics at or above this severity. Valid values:
        /// error, warning, info, hint. Omit to report everything (current
        /// behavior).
        #[arg(long, value_name = "LEVEL", value_parser = parse_min_severity)]
        min_severity: Option<ErrorSeverity>,
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
    /// Apply machine-applicable fixes for the curated fixable diagnostics.
    /// Dry-run by default (prints a unified-diff preview); pass `--apply` to write.
    Fix {
        /// Game identifier (hoi4, stellaris, eu4, ck2, ck3, vic2, vic3, ir, eu5, custom)
        #[arg(long, short)]
        game: String,
        /// Directory containing game files
        #[arg(long, short)]
        directory: PathBuf,
        /// Path to a .cwt rules file OR a directory containing .cwt rule files
        #[arg(long, short)]
        rules: PathBuf,
        /// Optional path to the base game install, indexed for reference
        /// resolution (see `validate --vanilla`).
        #[arg(long)]
        vanilla: Option<PathBuf>,
        /// Optional pre-generated vanilla index (see `cache-vanilla`).
        #[arg(long)]
        vanilla_cache: Option<PathBuf>,
        /// Extra filename glob patterns to skip. May be repeated.
        #[arg(long = "ignore-file", value_name = "GLOB")]
        ignore_files: Vec<String>,
        /// Extra directory glob patterns to skip. May be repeated.
        #[arg(long = "ignore-dir", value_name = "GLOB")]
        ignore_dirs: Vec<String>,
        /// Restrict loc validation/lookup to this language (repeatable).
        #[arg(long = "loc-language", value_name = "LANG", value_parser = parse_lang)]
        loc_language: Vec<Lang>,
        /// Only fix diagnostics with this CW code (repeatable). Omit to fix every
        /// fixable diagnostic. Example: --code CW282 --code CW280
        #[arg(long = "code", value_name = "CWxxx")]
        codes: Vec<String>,
        /// Write the fixes to disk. Without this the command is a dry run and
        /// prints a preview only.
        #[arg(long)]
        apply: bool,
    },
}

/// A fix to apply to one file: the underlying edit plus the diagnostic code /
/// title for the preview. Grouped per file by the `fix` subcommand.
struct PlannedFix {
    code: String,
    edit: cwtools_parser::fix::SpanEdit,
}

/// Resolve a planned file's edits: drop any that overlap an already-kept edit
/// (skip-and-warn, so a later edit never corrupts an earlier one), returning the
/// surviving edits in file order plus the codes of the skipped ones.
fn plan_file_edits(
    text: &str,
    mut planned: Vec<PlannedFix>,
) -> (Vec<cwtools_parser::fix::SpanEdit>, Vec<String>) {
    use cwtools_parser::fix::{line_start_bytes, pos_to_byte};
    let starts = line_start_bytes(text);
    // Sort by start byte ascending so overlap detection is a single forward scan.
    planned.sort_by_key(|p| pos_to_byte(text, &starts, p.edit.range.start));
    let mut kept: Vec<cwtools_parser::fix::SpanEdit> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    let mut last_end = 0usize;
    let mut first = true;
    for p in planned {
        let s = pos_to_byte(text, &starts, p.edit.range.start);
        let e = pos_to_byte(text, &starts, p.edit.range.end);
        if !first && s < last_end {
            skipped.push(p.code);
            continue;
        }
        last_end = e;
        first = false;
        kept.push(p.edit);
    }
    (kept, skipped)
}

/// A unified-diff-style preview of applying `edits` to `old` under `path`. One
/// hunk per edit (edits are already non-overlapping), showing the touched old
/// lines (`-`) and the resulting new lines (`+`).
fn fix_preview(path: &str, old: &str, edits: &[cwtools_parser::fix::SpanEdit]) -> String {
    use cwtools_parser::fix::{line_start_bytes, pos_to_byte};
    let starts = line_start_bytes(old);
    let line_of = |byte: usize| match starts.binary_search(&byte) {
        Ok(i) => i,
        Err(i) => i.saturating_sub(1),
    };
    let mut resolved: Vec<(usize, usize, &str)> = edits
        .iter()
        .map(|edit| {
            (
                pos_to_byte(old, &starts, edit.range.start),
                pos_to_byte(old, &starts, edit.range.end),
                edit.replacement.as_str(),
            )
        })
        .collect();
    resolved.sort_by_key(|r| r.0);

    let mut out = format!("--- {path}\n+++ {path}\n");
    for (s, e, repl) in resolved {
        let start_line = line_of(s);
        let end_line = if e > s { line_of(e - 1) } else { start_line };
        let hunk_start = starts[start_line];
        let hunk_end = starts.get(end_line + 1).copied().unwrap_or(old.len());
        let old_seg = &old[hunk_start..hunk_end];
        let new_seg = format!("{}{}{}", &old[hunk_start..s], repl, &old[e..hunk_end]);
        out.push_str(&format!("@@ -{} +{} @@\n", start_line + 1, start_line + 1));
        for l in old_seg.split_inclusive('\n') {
            out.push_str(&format!("-{}\n", l.strip_suffix('\n').unwrap_or(l)));
        }
        for l in new_seg.split_inclusive('\n') {
            out.push_str(&format!("+{}\n", l.strip_suffix('\n').unwrap_or(l)));
        }
    }
    out
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

/// One rendered diagnostic row for the `validate` report. Reads only
/// file/severity/code/message/line/hash — never a diagnostic's `fix`, so a
/// `SuggestedFix` payload is inert here (locked in by `fix_payload_is_inert`).
struct Diag {
    file: String,
    severity: cwtools_validation::ErrorSeverity,
    code: String,
    message: String,
    line: u32,
    hash: String,
}

/// Map a `ValidationError` to a report `Diag`, computing its hash. Consumes the
/// error (moves the message). The `fix` field is deliberately dropped.
fn validation_to_diag(file: &str, err: ValidationError) -> Diag {
    let code = err.code.unwrap_or_default().to_string();
    let hash = diag_hash(file, &code, &err.message, err.line);
    Diag {
        file: file.to_string(),
        severity: err.severity,
        code,
        message: err.message,
        line: err.line,
        hash,
    }
}

/// One CSV report row (trailing newline included).
fn csv_row(d: &Diag) -> String {
    format!(
        "{},{},{:?},{},{},{}\n",
        csv_escape(&d.file),
        d.line,
        d.severity,
        csv_escape(&d.code),
        csv_escape(&d.message),
        d.hash
    )
}

/// One JSON report row (trailing newline included); `last` suppresses the comma.
fn json_row(d: &Diag, last: bool) -> String {
    format!(
        "  {{\"file\":\"{}\",\"line\":{},\"severity\":\"{:?}\",\"code\":\"{}\",\"message\":\"{}\",\"hash\":\"{}\"}}{}\n",
        json_escape(&d.file),
        d.line,
        d.severity,
        json_escape(&d.code),
        json_escape(&d.message),
        d.hash,
        if last { "" } else { "," }
    )
}

/// One grouped-CLI report row (the per-diagnostic line, not the file header).
fn cli_row(d: &Diag) -> String {
    let code_part = if d.code.is_empty() {
        String::new()
    } else {
        format!("[{}] ", d.code)
    };
    format!(
        "    [{:?}] {}{} (line {})\n",
        d.severity, code_part, d.message, d.line
    )
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

/// Parse a `--loc-language` value into a `Lang`, for clap's `value_parser`.
fn parse_lang(s: &str) -> Result<Lang, String> {
    Lang::from_name(s).ok_or_else(|| {
        format!(
            "invalid language '{s}': valid values are english, french, german, spanish, russian, \
             polish, braz_por, simp_chinese, japanese, korean, turkish, default"
        )
    })
}

/// Parse a `--min-severity` value into an `ErrorSeverity`, for clap's `value_parser`.
fn parse_min_severity(s: &str) -> Result<ErrorSeverity, String> {
    match s.to_ascii_lowercase().as_str() {
        "error" => Ok(ErrorSeverity::Error),
        "warning" => Ok(ErrorSeverity::Warning),
        "info" => Ok(ErrorSeverity::Information),
        "hint" => Ok(ErrorSeverity::Hint),
        _ => Err(format!(
            "invalid severity '{s}': valid values are error, warning, info, hint"
        )),
    }
}

/// Ordinal rank for `--min-severity` filtering: higher is more severe.
fn severity_rank(s: ErrorSeverity) -> u8 {
    match s {
        ErrorSeverity::Error => 3,
        ErrorSeverity::Warning => 2,
        ErrorSeverity::Information => 1,
        ErrorSeverity::Hint => 0,
    }
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
            loc_language,
            min_severity,
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
                loc_languages: if loc_language.is_empty() {
                    None
                } else {
                    Some(loc_language)
                },
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

            // The driver validates files in parallel, in input order, so the
            // report is byte-for-byte identical to the sequential version.
            let ignored_ref = &ignored;
            let mut diags: Vec<Diag> = session
                .validate_all()
                .into_iter()
                .flat_map(|(path, errors)| {
                    let file_str = path.to_str().unwrap_or("").to_string();
                    errors.into_iter().filter_map(move |err| {
                        let d = validation_to_diag(&file_str, err);
                        if ignored_ref.contains(&d.hash) {
                            return None;
                        }
                        Some(d)
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

            // Same placement as the ignore_hashes filter above: strip diags
            // before they reach the error/warning counts, the report, and the
            // hash output. No-op unless --min-severity was passed.
            if let Some(min_sev) = min_severity {
                diags.retain(|d| severity_rank(d.severity) >= severity_rank(min_sev));
            }

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
                        out.push_str(&csv_row(d));
                    }
                }
                "json" => {
                    out.push_str("[\n");
                    for (i, d) in diags.iter().enumerate() {
                        out.push_str(&json_row(d, i + 1 >= diags.len()));
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
                        out.push_str(&cli_row(d));
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
            // Severity-aware like `validate`: a parse failure is always an
            // error; a lint diagnostic only counts if it's Error-severity, so
            // e.g. Information-severity CW234 placeholders don't fail CI.
            let total_errors = diags
                .iter()
                .filter(|d| d.severity == ErrorSeverity::Error)
                .count()
                + parse_errors.len();
            let code = exit_code(total_errors, false, false);
            if code != 0 {
                std::process::exit(code);
            }
        }
        Commands::Fix {
            game,
            directory,
            rules,
            vanilla,
            vanilla_cache,
            ignore_files,
            ignore_dirs,
            loc_language,
            codes,
            apply,
        } => {
            use cwtools_driver::{RulesInput, Session, SessionConfig};
            use cwtools_game::constants::Game;
            use std::collections::BTreeMap;

            let game_id = Game::from_str(&game).unwrap_or_else(|| {
                eprintln!("Unknown game: {}. Supported: hoi4, stellaris, eu4, ck2, ck3, vic2, vic3, ir, eu5, custom", game);
                std::process::exit(1);
            });

            // Uppercased code filter; empty means "every fixable diagnostic".
            let code_filter: std::collections::HashSet<String> =
                codes.iter().map(|c| c.to_ascii_uppercase()).collect();
            let want = |code: &str| code_filter.is_empty() || code_filter.contains(code);

            let vanilla_cache_index = vanilla_cache
                .as_ref()
                .and_then(|p| vanilla_cache::load(p).ok())
                .map(|(_, fp, data)| (fp, data));
            let (_fp, vanilla_cache_index) = vanilla_cache_index.unzip();

            let session = Session::load(SessionConfig {
                game: game_id,
                rules: RulesInput::from_path(rules.clone()),
                directory: directory.clone(),
                vanilla: vanilla.clone(),
                vanilla_cache: vanilla_cache_index,
                ignore_files: &ignore_files,
                ignore_dirs: &ignore_dirs,
                loc_languages: if loc_language.is_empty() {
                    None
                } else {
                    Some(loc_language)
                },
                on_rules_warning: Some(&mut |w: String| eprintln!("warn: {}", w)),
            });

            // Gather fixable diagnostics, grouped per file in deterministic order.
            let mut by_file: BTreeMap<String, Vec<PlannedFix>> = BTreeMap::new();
            for (path, errors) in session.validate_all() {
                let file_str = path.to_str().unwrap_or("").to_string();
                for err in errors {
                    let code = err.code.unwrap_or_default();
                    if !want(code) {
                        continue;
                    }
                    if let Some(fix) = err.fix {
                        for edit in fix.edits {
                            by_file
                                .entry(file_str.clone())
                                .or_default()
                                .push(PlannedFix {
                                    code: code.to_string(),
                                    edit,
                                });
                        }
                    }
                }
            }
            // Loc diagnostics: only mod-path files (mirror `validate`'s filter).
            let dir_prefix = {
                let s = directory.to_string_lossy();
                if s.ends_with(std::path::MAIN_SEPARATOR) {
                    s.into_owned()
                } else {
                    format!("{}{}", s, std::path::MAIN_SEPARATOR)
                }
            };
            for d in session.loc_project_diagnostics() {
                if !d.file.starts_with(&dir_prefix) || !want(d.code) {
                    continue;
                }
                if let Some(fix) = d.fix {
                    for edit in fix.edits {
                        by_file.entry(d.file.clone()).or_default().push(PlannedFix {
                            code: d.code.to_string(),
                            edit,
                        });
                    }
                }
            }

            let mut files_changed = 0usize;
            let mut edits_applied = 0usize;
            let mut write_failed = false;
            for (file, planned) in by_file {
                let Ok(text) = std::fs::read_to_string(&file) else {
                    eprintln!("warn: could not read {file}; skipping its fixes");
                    continue;
                };
                let (kept, skipped) = plan_file_edits(&text, planned);
                for code in &skipped {
                    eprintln!("warn: {file}: skipped a {code} fix (overlaps another edit)");
                }
                if kept.is_empty() {
                    continue;
                }
                if apply {
                    let fixed = cwtools_parser::fix::apply_edits(&text, &kept);
                    if let Err(e) = std::fs::write(&file, &fixed) {
                        eprintln!("Error writing {file}: {e}");
                        write_failed = true;
                    } else {
                        files_changed += 1;
                        edits_applied += kept.len();
                        println!("fixed {file} ({} edit(s))", kept.len());
                    }
                } else {
                    print!("{}", fix_preview(&file, &text, &kept));
                    files_changed += 1;
                    edits_applied += kept.len();
                }
            }

            if apply {
                println!(
                    "\nApplied {} fix(es) across {} file(s)",
                    edits_applied, files_changed
                );
            } else {
                println!(
                    "\nDry run: {} fix(es) across {} file(s) would be applied (pass --apply to write)",
                    edits_applied, files_changed
                );
            }

            if write_failed {
                std::process::exit(2);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cwtools_parser::ast::{SourcePos, SourceRange};
    use cwtools_parser::fix::{SpanEdit, SuggestedFix};

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

    fn err_base() -> ValidationError {
        ValidationError {
            message: "redundant default, remove it".to_string(),
            severity: ErrorSeverity::Information,
            line: 12,
            col: 4,
            file: "common/ideas/x.txt".to_string(),
            code: Some("CW282"),
            fix: None,
        }
    }

    // Inertness guard (Task 8, step 2): a fix payload must not change the report.
    // The `Diag` mapping and every report row read no fix data — locked in here so
    // populating the emit sites keeps validate output byte-identical.
    #[test]
    fn fix_payload_is_inert_in_report() {
        let base = err_base();
        let mut with_fix = base.clone();
        with_fix.fix = Some(SuggestedFix::delete(
            "Remove redundant default",
            SourceRange {
                start: SourcePos { line: 12, col: 4 },
                end: SourcePos { line: 13, col: 0 },
            },
        ));

        let d0 = validation_to_diag(&base.file.clone(), base);
        let d1 = validation_to_diag(&with_fix.file.clone(), with_fix);

        assert_eq!(d0.hash, d1.hash, "hash must ignore the fix");
        assert_eq!(csv_row(&d0), csv_row(&d1), "csv row must ignore the fix");
        assert_eq!(json_row(&d0, true), json_row(&d1, true));
        assert_eq!(cli_row(&d0), cli_row(&d1), "cli row must ignore the fix");
    }

    fn edit(l0: u32, c0: u16, l1: u32, c1: u16, repl: &str) -> SpanEdit {
        SpanEdit {
            range: SourceRange {
                start: SourcePos { line: l0, col: c0 },
                end: SourcePos { line: l1, col: c1 },
            },
            replacement: repl.to_string(),
        }
    }

    // Step 5: multi-edit-per-file ordering. Two non-overlapping edits on one file
    // apply to the same result regardless of the order they were queued.
    #[test]
    fn multiple_edits_per_file_apply_in_descending_order() {
        let text = "aaaa bbbb\n";
        let forward = vec![
            PlannedFix {
                code: "CWA".into(),
                edit: edit(1, 0, 1, 4, "X"),
            },
            PlannedFix {
                code: "CWB".into(),
                edit: edit(1, 5, 1, 9, "Y"),
            },
        ];
        let reversed = vec![
            PlannedFix {
                code: "CWB".into(),
                edit: edit(1, 5, 1, 9, "Y"),
            },
            PlannedFix {
                code: "CWA".into(),
                edit: edit(1, 0, 1, 4, "X"),
            },
        ];
        for planned in [forward, reversed] {
            let (kept, skipped) = plan_file_edits(text, planned);
            assert!(skipped.is_empty(), "no overlap expected");
            assert_eq!(kept.len(), 2);
            assert_eq!(cwtools_parser::fix::apply_edits(text, &kept), "X Y\n");
        }
    }

    // Step 5: overlap skip. When two edits overlap, the later one is dropped (and
    // reported) so it can't corrupt the kept edit.
    #[test]
    fn overlapping_edits_skip_and_warn() {
        let text = "aaaa bbbb\n";
        let planned = vec![
            PlannedFix {
                code: "CWA".into(),
                edit: edit(1, 0, 1, 6, "X"), // covers "aaaa b"
            },
            PlannedFix {
                code: "CWB".into(),
                edit: edit(1, 5, 1, 9, "Y"), // overlaps at col 5
            },
        ];
        let (kept, skipped) = plan_file_edits(text, planned);
        assert_eq!(kept.len(), 1, "one edit kept");
        assert_eq!(skipped, vec!["CWB".to_string()], "overlapping edit skipped");
        assert_eq!(cwtools_parser::fix::apply_edits(text, &kept), "Xbbb\n");
    }
}
