use clap::{CommandFactory, Parser, Subcommand};
use cwtools_driver::{index_game_dir, search_config_for};
use cwtools_file_manager::file_manager::{FileManager, FileManagerConfig};
use cwtools_localization::Lang;
use cwtools_parser::parser::parse_string;
use cwtools_rules::rules_types::RuleSet;
use cwtools_rules::ruleset_loader::load_ruleset_from_dir;
use cwtools_string_table::string_table::StringTable;
use cwtools_validation::{ErrorSeverity, ValidationError};
use std::borrow::Cow;
use std::path::{Path, PathBuf};

use cwtools_info::vanilla_cache;

mod codes;
mod config;
mod report;

use report::ReportType;

#[derive(Parser)]
#[command(name = "cwtools")]
#[command(about = "CWTools CLI — Paradox mod tooling")]
// From CARGO_PKG_VERSION, the same source `cwtools-server --version` prints.
#[command(version)]
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
        /// Read settings from this cwtools.toml instead of searching for one.
        ///
        /// Without it, the first `cwtools.toml` at or above --directory (or the
        /// working directory) is used. Flags override file values; the boolean
        /// switches are the exception — they can only add to a `true` in the
        /// file, never turn one off.
        /// Recognised keys: game, directory, rules, vanilla, vanilla-cache,
        /// no-vanilla-cache, refresh-vanilla-cache, report-type, min-severity,
        /// ignore-files, ignore-dirs, loc-languages, ignore-codes, only-codes,
        /// allow-empty. Relative paths resolve against the config file.
        #[arg(long, value_name = "FILE")]
        config: Option<PathBuf>,
        /// Game identifier (hoi4, stellaris, eu4, ck2, ck3, vic2, vic3, ir, eu5, custom)
        #[arg(long, short)]
        game: Option<String>,
        /// Directory containing game files. A single mod root is validated as-is.
        /// A workspace of mods (a directory that is not itself a mod root but whose
        /// `mod/`/`mods/` folder holds `.mod` descriptors) is auto-detected and
        /// expanded: every referenced mod is validated together, layered by load
        /// order (a later-resolved mod overrides a shared logical path; a mod's
        /// `replace_path` suppresses lower-priority files under that prefix).
        #[arg(long, short)]
        directory: Option<PathBuf>,
        /// Path to a .cwt rules file OR a directory containing .cwt rule files
        #[arg(long, short)]
        rules: Option<PathBuf>,
        /// Optional path to the base game install (e.g. the vanilla HOI4 folder).
        /// Its files are indexed for reference resolution but not validated, so a
        /// mod can reference base-game content (operation_tokens, ship_names, …)
        /// without false "not a known instance" errors. The index is cached under
        /// the OS cache dir (XDG_CACHE_HOME/cwtools, %LOCALAPPDATA%\cwtools,
        /// ~/.cache/cwtools) and reused while the install and rules are unchanged;
        /// see --no-vanilla-cache / --refresh-vanilla-cache.
        #[arg(long)]
        vanilla: Option<PathBuf>,
        /// Optional pre-generated vanilla index (see `cache-vanilla`). Loaded for
        /// reference resolution without re-parsing the game install. Faster than
        /// `--vanilla`; can be combined with it.
        #[arg(long)]
        vanilla_cache: Option<PathBuf>,
        /// Don't read or write the automatic base-game cache: re-parse the
        /// `--vanilla` install on every run.
        #[arg(long)]
        no_vanilla_cache: bool,
        /// Ignore any existing automatic base-game cache, re-parse the
        /// `--vanilla` install, and overwrite the cache with the result.
        #[arg(long)]
        refresh_vanilla_cache: bool,
        /// Report format: cli (default, grouped text), csv, json, github
        /// (Actions workflow commands, annotating the PR diff), or sarif
        /// (SARIF 2.1.0 for code-scanning upload).
        #[arg(long, value_name = "FORMAT", value_parser = report::parse_report_type)]
        report_type: Option<ReportType>,
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
        /// Enforce exact on-disk case for CW113 `filepath` references against the
        /// mod's own files. Off by default (Windows-authored mods); enable when
        /// the mod must also load on a case-sensitive filesystem (Linux/Mac).
        #[arg(long)]
        case_sensitive_files: bool,
        /// Only report diagnostics at or above this severity. Valid values:
        /// error, warning, info, hint. Omit to report everything (current
        /// behavior).
        #[arg(long, value_name = "LEVEL", value_parser = parse_min_severity)]
        min_severity: Option<ErrorSeverity>,
        /// Drop every diagnostic with this CW code (repeatable). The same
        /// suppression the editor applies via `cwtools.errors.ignore`, so one
        /// policy can cover both. Example: --ignore-code CW100
        #[arg(long = "ignore-code", value_name = "CWxxx", value_parser = codes::parse_code)]
        ignore_codes: Vec<String>,
        /// Report only diagnostics with this CW code (repeatable). Omit to
        /// report every code. `--ignore-code` still applies on top.
        #[arg(long = "only-code", value_name = "CWxxx", value_parser = codes::parse_code)]
        only_codes: Vec<String>,
        /// Accept a run with nothing to validate. Without this, a ruleset that
        /// loads no types or a directory that yields no files is an error
        /// (exit 4) instead of a silent "0 errors".
        #[arg(long)]
        allow_empty: bool,
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
        directory: Option<PathBuf>,
        /// Read settings from this cwtools.toml instead of searching for one.
        /// `loc` reads directory, report-type, min-severity, ignore-codes,
        /// only-codes and allow-empty; see `validate --help` for the schema.
        #[arg(long, value_name = "FILE")]
        config: Option<PathBuf>,
        /// Report format: cli (default, grouped text), csv, json, github
        /// (Actions workflow commands), or sarif (SARIF 2.1.0).
        #[arg(long, value_name = "FORMAT", value_parser = report::parse_report_type)]
        report_type: Option<ReportType>,
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
        /// Only report diagnostics at or above this severity. Valid values:
        /// error, warning, info, hint. Omit to report everything.
        #[arg(long, value_name = "LEVEL", value_parser = parse_min_severity)]
        min_severity: Option<ErrorSeverity>,
        /// Drop every diagnostic with this CW code (repeatable).
        #[arg(long = "ignore-code", value_name = "CWxxx", value_parser = codes::parse_code)]
        ignore_codes: Vec<String>,
        /// Report only diagnostics with this CW code (repeatable).
        #[arg(long = "only-code", value_name = "CWxxx", value_parser = codes::parse_code)]
        only_codes: Vec<String>,
        /// Accept a run with nothing to check. Without this, a directory that
        /// holds no localisation files is an error (exit 4).
        #[arg(long)]
        allow_empty: bool,
    },
    /// Apply machine-applicable fixes for the curated fixable diagnostics.
    /// Dry-run by default (prints a unified-diff preview); pass `--apply` to write.
    Fix {
        /// Read settings from this cwtools.toml instead of searching for one.
        /// `fix` reads every key `validate` does except report-type and
        /// min-severity; see `validate --help` for the schema.
        #[arg(long, value_name = "FILE")]
        config: Option<PathBuf>,
        /// Game identifier (hoi4, stellaris, eu4, ck2, ck3, vic2, vic3, ir, eu5, custom)
        #[arg(long, short)]
        game: Option<String>,
        /// Directory containing game files
        #[arg(long, short)]
        directory: Option<PathBuf>,
        /// Path to a .cwt rules file OR a directory containing .cwt rule files
        #[arg(long, short)]
        rules: Option<PathBuf>,
        /// Optional path to the base game install, indexed for reference
        /// resolution (see `validate --vanilla`).
        #[arg(long)]
        vanilla: Option<PathBuf>,
        /// Optional pre-generated vanilla index (see `cache-vanilla`).
        #[arg(long)]
        vanilla_cache: Option<PathBuf>,
        /// Don't read or write the automatic base-game cache (see `validate`).
        #[arg(long)]
        no_vanilla_cache: bool,
        /// Ignore any existing automatic base-game cache and overwrite it.
        #[arg(long)]
        refresh_vanilla_cache: bool,
        /// Extra filename glob patterns to skip. May be repeated.
        #[arg(long = "ignore-file", value_name = "GLOB")]
        ignore_files: Vec<String>,
        /// Extra directory glob patterns to skip. May be repeated.
        #[arg(long = "ignore-dir", value_name = "GLOB")]
        ignore_dirs: Vec<String>,
        /// Restrict loc validation/lookup to this language (repeatable).
        #[arg(long = "loc-language", value_name = "LANG", value_parser = parse_lang)]
        loc_language: Vec<Lang>,
        /// Enforce exact on-disk case for CW113 `filepath` references against the
        /// mod's own files. Off by default (Windows-authored mods); enable when
        /// the mod must also load on a case-sensitive filesystem (Linux/Mac).
        #[arg(long)]
        case_sensitive_files: bool,
        /// Only fix diagnostics with this CW code (repeatable). Omit to fix every
        /// fixable diagnostic. Example: --code CW282 --code CW280
        #[arg(long = "code", value_name = "CWxxx")]
        codes: Vec<String>,
        /// Write the fixes to disk. Without this the command is a dry run and
        /// prints a preview only.
        #[arg(long)]
        apply: bool,
        /// Accept a run with nothing to fix. Without this, a ruleset that loads
        /// no types or a directory that yields no files is an error (exit 4).
        #[arg(long)]
        allow_empty: bool,
    },
}

/// A fix to apply to one file: the diagnostic code (for the skip warning) paired
/// with the underlying edit. Grouped per file by the `fix` subcommand and handed
/// to `cwtools_parser::fix::plan_file_edits`, which owns the overlap resolution
/// the LSP `source.fixAll` action shares.
type PlannedFix = (String, cwtools_parser::fix::SpanEdit);

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

/// FNV-1a-64 hex digest of `parts`, joined by `|`. FNV rather than std's
/// `DefaultHasher` because the seed there is randomized per process: a baseline
/// file has to mean the same thing on every run and every machine.
fn fnv1a_digest(parts: [&str; 4]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    let mut mix = |b: u8| {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    };
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            mix(b'|');
        }
        for b in part.bytes() {
            mix(b);
        }
    }
    format!("{:016x}", h)
}

/// `file` relative to `root` and `/`-separated, for hashing: a baseline must
/// mean the same thing whether `file` came out absolute or mod-relative.
/// Falls back to `file` (still `/`-separated) when it isn't under `root` — a
/// vanilla install path reported alongside mod files, say — rather than
/// panicking. Lexical only, no filesystem access, so a file that was never
/// written to disk (as in tests) still hashes.
fn relative_file(file: &str, root: &Path) -> String {
    let file = file.replace('\\', "/");
    let root = root.to_string_lossy().replace('\\', "/");
    match Path::new(&file).strip_prefix(Path::new(&root)) {
        Ok(rel) => rel.to_string_lossy().replace('\\', "/"),
        Err(_) => file,
    }
}

/// Stable digest of a diagnostic, for baseline/ignore matching. Keyed on the
/// trimmed text of the offending source line rather than its line number, so
/// inserting a line above a baselined diagnostic doesn't resurface it as new.
/// Two identical diagnostics on two identical lines of one file collapse to one
/// digest, which is the intended trade: baselines track content, not position.
/// `file` is relativized against `root` first, so the digest doesn't depend on
/// whether this invocation happened to see an absolute or relative path.
fn diag_hash(root: &Path, file: &str, code: &str, message: &str, line_text: &str) -> String {
    fnv1a_digest([relative_file(file, root).as_str(), code, message, line_text])
}

/// The previous digest, keyed on the line number. Still accepted when matching
/// `--ignore-hashes` so existing baselines don't all invalidate at once; never
/// emitted. Remove once the migration window closes.
fn legacy_diag_hash(root: &Path, file: &str, code: &str, message: &str, line: u32) -> String {
    fnv1a_digest([
        relative_file(file, root).as_str(),
        code,
        message,
        &line.to_string(),
    ])
}

/// One-slot memo of a file's trimmed lines, feeding [`diag_hash`]. Diagnostics
/// arrive grouped by file from both the validation and loc passes, so a single
/// slot keeps each file to one read and only one file resident.
#[derive(Default)]
struct SourceLines {
    file: String,
    lines: Vec<String>,
}

impl SourceLines {
    /// Trimmed text of 1-based `line` in `file`; `""` when the file can't be
    /// read or the line doesn't exist (whole-file diagnostics report line 0).
    fn trimmed(&mut self, file: &str, line: u32) -> &str {
        if self.file != file {
            self.lines = std::fs::read_to_string(file)
                .map(|text| text.lines().map(|l| l.trim().to_string()).collect())
                .unwrap_or_default();
            self.file = file.to_string();
        }
        line.checked_sub(1)
            .and_then(|i| self.lines.get(i as usize))
            .map_or("", String::as_str)
    }
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
    file: cwtools_validation::FilePath,
    severity: cwtools_validation::ErrorSeverity,
    /// The catalog id (`""` for a diagnostic with no code). Both sources hand
    /// one out as `&'static str`, so the row never owns a copy.
    code: &'static str,
    message: String,
    line: u32,
    /// 1-based column, normalised from the emitting subsystem's convention.
    /// Only the `github` and `sarif` reports read it, so it stays out of the
    /// hash and the cli/csv/json rows.
    col: u32,
    hash: String,
    /// The previous line-number digest, for matching older baselines only.
    /// Empty unless an `--ignore-hashes` baseline was loaded — nothing else
    /// reads it, and computing one costs another relativize-and-digest per
    /// diagnostic.
    legacy_hash: String,
}

/// Whether a `--ignore-hashes` baseline suppresses `d`. Both digests are
/// accepted for the migration window: baselines written before the digest
/// became content-derived keep matching, while only the new one is ever
/// emitted, so a rewritten baseline converts itself.
fn is_ignored(ignored: &std::collections::HashSet<String>, d: &Diag) -> bool {
    ignored.contains(&d.hash) || ignored.contains(&d.legacy_hash)
}

/// The legacy line-number digest, or an empty string when no baseline is loaded
/// and nothing will ever compare against it.
fn legacy_hash_if_wanted(
    wanted: bool,
    root: &Path,
    file: &str,
    code: &str,
    message: &str,
    line: u32,
) -> String {
    if wanted {
        legacy_diag_hash(root, file, code, message, line)
    } else {
        String::new()
    }
}

/// Map a `ValidationError` to a report `Diag`, computing its hash from the
/// trimmed source line. Consumes the error (moves the message and the shared
/// file path). The `fix` field is deliberately dropped. `root` is the mod root
/// the hash is relativized against; the emitted `file` column is untouched.
/// `legacy` requests the older line-number digest, needed only when matching an
/// `--ignore-hashes` baseline.
fn validation_to_diag(root: &Path, err: ValidationError, line_text: &str, legacy: bool) -> Diag {
    let code = err.code.unwrap_or_default();
    let hash = diag_hash(root, &err.file, code, &err.message, line_text);
    let legacy_hash = legacy_hash_if_wanted(legacy, root, &err.file, code, &err.message, err.line);
    Diag {
        file: err.file,
        severity: err.severity,
        code,
        message: err.message,
        line: err.line,
        // Parser columns are 0-based; both CI formats are 1-based.
        col: err.col as u32 + 1,
        hash,
        legacy_hash,
    }
}

/// Map a `LocDiagnostic` to a report `Diag`, computing its hash. Consumes the
/// diagnostic (moves file/message). The `fix` field is deliberately dropped,
/// same as `validation_to_diag`. `root` is the mod root the hash is
/// relativized against.
fn loc_diagnostic_to_diag(
    root: &Path,
    d: cwtools_localization::LocDiagnostic,
    line_text: &str,
    legacy: bool,
) -> Diag {
    let line = d.line as u32;
    let hash = diag_hash(root, &d.file, d.code, &d.message, line_text);
    let legacy_hash = legacy_hash_if_wanted(legacy, root, &d.file, d.code, &d.message, line);
    Diag {
        file: d.file.as_str().into(),
        severity: d.severity,
        code: d.code,
        message: d.message,
        line,
        // Loc diagnostics already count columns from 1.
        col: (d.col as u32).max(1),
        hash,
        legacy_hash,
    }
}

/// Map a `LocService` fatal parse error (a file that couldn't even be
/// lenient-parsed, so there's no line number) to a report `Diag`. Always
/// Error-severity; `line` is 0 like other whole-file diagnostics, so there's no
/// source line to key the hash on. `root` is the mod root the hash is
/// relativized against.
fn loc_parse_error_to_diag(root: &Path, file: String, message: String, legacy: bool) -> Diag {
    let hash = diag_hash(root, &file, "", &message, "");
    let legacy_hash = legacy_hash_if_wanted(legacy, root, &file, "", &message, 0);
    Diag {
        file: file.as_str().into(),
        severity: ErrorSeverity::Error,
        code: "",
        message,
        line: 0,
        col: 1,
        hash,
        legacy_hash,
    }
}

/// One CSV report row (trailing newline included).
fn csv_row(d: &Diag) -> String {
    format!(
        "{},{},{:?},{},{},{}\n",
        csv_escape(&d.file),
        d.line,
        d.severity,
        csv_escape(d.code),
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
        json_escape(d.code),
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

/// The file walk itself failed (the path doesn't resolve, a dir is unreadable).
const EXIT_DISCOVERY_FAILED: i32 = 3;

/// An input resolved to nothing: a ruleset with no types, or a target directory
/// with no files. Distinct from [`EXIT_DISCOVERY_FAILED`] so CI can tell
/// "nothing to check" from "the walk errored".
const EXIT_EMPTY_INPUT: i32 = 4;

/// A `cwtools.toml` that couldn't be read or understood. Shares clap's
/// usage-error code: the run never started, so it is not a validation result.
const EXIT_CONFIG_ERROR: i32 = 2;

/// Resolve the run's config file, failing loudly on a broken one. `anchor` is
/// the directory the upward search starts from when `--config` wasn't given.
fn load_config(explicit: Option<&Path>, anchor: Option<&Path>) -> Option<config::FileConfig> {
    config::resolve(explicit, anchor).unwrap_or_else(|e| {
        eprintln!("error: {e}");
        std::process::exit(EXIT_CONFIG_ERROR);
    })
}

/// Whether stdout is carrying one of the CI report formats, in which case status
/// lines have to go to stderr instead: `cwtools loc . --report-type sarif >
/// out.sarif` must not have a progress banner in the middle of the JSON. Only
/// the two new formats divert — cli, csv and json keep every line where it was.
fn report_owns_stdout(report_type: ReportType, output_file: Option<&PathBuf>) -> bool {
    output_file.is_none() && matches!(report_type, ReportType::Github | ReportType::Sarif)
}

/// A progress/status line, diverted to stderr when the report owns stdout.
fn status(line: String, to_stderr: bool) {
    if to_stderr {
        eprintln!("{line}");
    } else {
        println!("{line}");
    }
}

/// Report which config file the run used and what it contributed, on stderr so
/// a redirected report stays clean. `reads` is the running subcommand's key set:
/// anything the file sets outside it is named, since a key that quietly does
/// nothing is the failure mode a shared config invites.
fn announce_config(
    subcommand: &str,
    cfg: Option<&config::FileConfig>,
    applied: &[&'static str],
    reads: &[&str],
) {
    let Some(cfg) = cfg else { return };
    let what = if applied.is_empty() {
        "no settings applied".to_string()
    } else {
        format!("applied: {}", applied.join(", "))
    };
    eprintln!("Using config {} ({})", cfg.path.display(), what);
    let unread: Vec<&str> = cfg
        .present
        .iter()
        .copied()
        .filter(|k| !reads.contains(k))
        .collect();
    if !unread.is_empty() {
        eprintln!(
            "warn: {} sets {}, which `{subcommand}` does not read",
            cfg.path.display(),
            unread.join(", ")
        );
    }
}

/// Bail on a setting that neither a flag nor the config file supplied, through
/// clap so the message, the usage line and the exit code match every other
/// usage error.
fn missing_required(subcommand: &str, arg: &str, key: &str, cfg: Option<&config::FileConfig>) -> ! {
    let hint = match cfg {
        Some(c) => format!("{} does not set `{key}`", c.path.display()),
        None => format!(
            "no {} was found; `{key}` could come from one",
            config::FILE_NAME
        ),
    };
    let kind = clap::error::ErrorKind::MissingRequiredArgument;
    let message = format!("the following required argument was not provided: {arg}\n\n  {hint}");
    let mut root = Cli::command();
    if let Some(sub) = root.find_subcommand_mut(subcommand) {
        sub.error(kind, message).exit()
    }
    root.error(kind, message).exit()
}

/// Map a run's outcome to a process exit code. Operational failures (couldn't
/// discover the files, couldn't write the report) are distinct from validation
/// finding errors, so CI can tell "the tool couldn't run" apart from "validation
/// found problems". 0 = clean run, no errors.
fn exit_code(total_errors: usize, discovery_failed: bool, write_failed: bool) -> i32 {
    if discovery_failed {
        EXIT_DISCOVERY_FAILED
    } else if write_failed {
        2
    } else if total_errors > 0 {
        1
    } else {
        0
    }
}

/// A path as the run resolved it: absolute where that can be computed, so an
/// error names the location a relative CI path actually pointed at.
fn resolved_path(path: &std::path::Path) -> String {
    std::path::absolute(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
}

/// Message for an input that resolved to nothing. `what` names the input and
/// what came back empty; the path is what the run resolved it to.
fn empty_input_error(what: &str, path: &std::path::Path) -> String {
    format!(
        "error: {what}: {} (nothing to check; pass --allow-empty if this is intended)",
        resolved_path(path)
    )
}

/// Fail loudly when an input resolved to nothing. Validating against an empty
/// ruleset, or over an empty file set, reports "0 errors" and exits 0, which
/// leaves a CI job with a typo'd path permanently green.
fn exit_if_empty(count: usize, allow_empty: bool, what: &str, path: &std::path::Path) {
    if count == 0 && !allow_empty {
        eprintln!("{}", empty_input_error(what, path));
        std::process::exit(EXIT_EMPTY_INPUT);
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
                        // The parser recovers rather than bailing, so a malformed
                        // file still returns Ok with a partial AST. Reporting only
                        // the summary made `a = { b =` look clean.
                        if !parsed.errors.is_empty() {
                            eprintln!(
                                "\n{} parse error(s) in {}:",
                                parsed.errors.len(),
                                file.display()
                            );
                            for e in &parsed.errors {
                                eprintln!("  {}", e);
                            }
                            std::process::exit(1);
                        }
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
                Ok(Ok((arena, root))) => {
                    println!("Deserialized from {}", input.display());
                    println!("  Leaves:   {}", arena.leaves.len());
                    println!("  Values:   {}", arena.leaf_values.len());
                    println!("  Comments: {}", arena.comments.len());
                    println!("  Root children: {}", root.len());
                }
                Ok(Err(e)) | Err(e) => {
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
            config,
            game,
            directory,
            rules,
            vanilla,
            vanilla_cache,
            no_vanilla_cache,
            refresh_vanilla_cache,
            report_type,
            output_file,
            ignore_hashes,
            output_hashes,
            ignore_files,
            ignore_dirs,
            loc_language,
            case_sensitive_files,
            min_severity,
            ignore_codes,
            only_codes,
            allow_empty,
        } => {
            use cwtools_driver::{RulesInput, Session, SessionConfig, VanillaCacheAuto};
            use cwtools_game::constants::Game;

            let file_cfg = load_config(config.as_deref(), directory.as_deref());
            let mut applied: Vec<&'static str> = Vec::new();
            let fc = file_cfg.as_ref();
            let game = config::pick(game, fc.and_then(|c| c.game.clone()), "game", &mut applied);
            let directory = config::pick(
                directory,
                fc.and_then(|c| c.directory.clone()),
                "directory",
                &mut applied,
            );
            let rules = config::pick(
                rules,
                fc.and_then(|c| c.rules.clone()),
                "rules",
                &mut applied,
            );
            let vanilla = config::pick(
                vanilla,
                fc.and_then(|c| c.vanilla.clone()),
                "vanilla",
                &mut applied,
            );
            let vanilla_cache = config::pick(
                vanilla_cache,
                fc.and_then(|c| c.vanilla_cache.clone()),
                "vanilla-cache",
                &mut applied,
            );
            let no_vanilla_cache = config::pick_flag(
                no_vanilla_cache,
                fc.is_some_and(|c| c.no_vanilla_cache),
                "no-vanilla-cache",
                &mut applied,
            );
            let refresh_vanilla_cache = config::pick_flag(
                refresh_vanilla_cache,
                fc.is_some_and(|c| c.refresh_vanilla_cache),
                "refresh-vanilla-cache",
                &mut applied,
            );
            let case_sensitive_files = config::pick_flag(
                case_sensitive_files,
                fc.is_some_and(|c| c.case_sensitive_files),
                "case-sensitive-files",
                &mut applied,
            );
            let report_type = config::pick(
                report_type,
                fc.and_then(|c| c.report_type),
                "report-type",
                &mut applied,
            )
            .unwrap_or(ReportType::Cli);
            let min_severity = config::pick(
                min_severity,
                fc.and_then(|c| c.min_severity),
                "min-severity",
                &mut applied,
            );
            let ignore_files = config::pick_list(
                ignore_files,
                fc.map(|c| c.ignore_files.clone()).unwrap_or_default(),
                "ignore-files",
                &mut applied,
            );
            let ignore_dirs = config::pick_list(
                ignore_dirs,
                fc.map(|c| c.ignore_dirs.clone()).unwrap_or_default(),
                "ignore-dirs",
                &mut applied,
            );
            let loc_language = config::pick_list(
                loc_language,
                fc.map(|c| c.loc_languages.clone()).unwrap_or_default(),
                "loc-languages",
                &mut applied,
            );
            let ignore_codes = config::pick_list(
                ignore_codes,
                fc.map(|c| c.ignore_codes.clone()).unwrap_or_default(),
                "ignore-codes",
                &mut applied,
            );
            let only_codes = config::pick_list(
                only_codes,
                fc.map(|c| c.only_codes.clone()).unwrap_or_default(),
                "only-codes",
                &mut applied,
            );
            let allow_empty = config::pick_flag(
                allow_empty,
                fc.is_some_and(|c| c.allow_empty),
                "allow-empty",
                &mut applied,
            );
            announce_config("validate", fc, &applied, config::VALIDATE_KEYS);

            let game =
                game.unwrap_or_else(|| missing_required("validate", "--game <GAME>", "game", fc));
            let directory = directory.unwrap_or_else(|| {
                missing_required("validate", "--directory <DIRECTORY>", "directory", fc)
            });
            let rules = rules
                .unwrap_or_else(|| missing_required("validate", "--rules <RULES>", "rules", fc));

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

            // Without an explicit --vanilla-cache, keep one under the OS cache dir
            // so repeat runs don't re-parse the whole install. The driver keys it
            // on game version + ruleset shape and rebuilds it when either moves.
            let vanilla_cache_auto = if no_vanilla_cache || vanilla_cache.is_some() {
                None
            } else {
                cwtools_driver::default_cache_dir().map(|dir| VanillaCacheAuto {
                    dir,
                    refresh: refresh_vanilla_cache,
                })
            };

            // Build the whole engine pipeline through the shared driver: parse
            // rules, discover/parse mod files, build the type/var/vanilla indexes,
            // expand modifier keys, build the loc index, prebuild the scope
            // registry. The CLI and LSP share this one implementation.
            let session = Session::load_with_parse_cache(
                SessionConfig {
                    game: game_id,
                    rules: RulesInput::from_path(rules.clone()),
                    directory: directory.clone(),
                    vanilla: vanilla.clone(),
                    vanilla_cache: vanilla_cache_index,
                    vanilla_cache_auto,
                    ignore_files: &ignore_files,
                    ignore_dirs: &ignore_dirs,
                    loc_languages: if loc_language.is_empty() {
                        None
                    } else {
                        Some(loc_language)
                    },
                    case_sensitive_files,
                    on_rules_warning: Some(&mut |w: String| eprintln!("warn: {}", w)),
                },
                cwtools_driver::default_cache_dir(),
            );
            let ruleset = session.ruleset();
            eprintln!(
                "  Loaded {} types, {} enums, {} aliases",
                ruleset.types.len(),
                ruleset.enums.len(),
                ruleset.aliases.len()
            );
            eprintln!("  Discovered {} files", session.parsed_files().len());

            // Nothing to validate is a failure, not a clean run. A failed walk
            // already exits 3 below, so don't relabel it as an empty input.
            if !session.discovery_failed {
                exit_if_empty(
                    ruleset.types.len(),
                    allow_empty,
                    "--rules loaded 0 types",
                    &rules,
                );
                exit_if_empty(
                    session.parsed_files().len(),
                    allow_empty,
                    "--directory contains no files to validate",
                    &directory,
                );
            }

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
            let want_legacy_hash = !ignored.is_empty();
            let mut sources = SourceLines::default();
            let mut diags: Vec<Diag> = Vec::new();
            for (path, errors) in session.validate_all() {
                let file_str = path.to_str().unwrap_or("");
                for err in errors {
                    // Same placement as the hash baseline: a suppressed code
                    // never reaches the counts, the report or --output-hashes.
                    if !codes::wanted(err.code.unwrap_or_default(), &only_codes, &ignore_codes) {
                        continue;
                    }
                    let line_text = sources.trimmed(file_str, err.line);
                    let d = validation_to_diag(&directory, err, line_text, want_legacy_hash);
                    if is_ignored(&ignored, &d) {
                        continue;
                    }
                    diags.push(d);
                }
            }
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
                if !d.file.starts_with(&dir_prefix)
                    || !codes::wanted(d.code, &only_codes, &ignore_codes)
                {
                    continue;
                }
                let line_text = sources.trimmed(&d.file, d.line as u32).to_string();
                let d = loc_diagnostic_to_diag(&directory, d, &line_text, want_legacy_hash);
                if is_ignored(&ignored, &d) {
                    continue;
                }
                diags.push(d);
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
                let type_instances: usize = type_index.map.values().map(|v| v.len()).sum();
                eprintln!(
                    "  [profile]   parsed ASTs released after indexing ({} files)",
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
            match report_type {
                ReportType::Csv => {
                    out.push_str("file,line,severity,code,message,hash\n");
                    for d in &diags {
                        out.push_str(&csv_row(d));
                    }
                }
                ReportType::Json => {
                    out.push_str("[\n");
                    for (i, d) in diags.iter().enumerate() {
                        out.push_str(&json_row(d, i + 1 >= diags.len()));
                    }
                    out.push_str("]\n");
                }
                ReportType::Github => {
                    let root = report::report_root();
                    for d in &diags {
                        out.push_str(&report::github_row(d, &root));
                    }
                }
                ReportType::Sarif => {
                    let refs: Vec<&Diag> = diags.iter().collect();
                    out.push_str(&report::sarif_report(&refs, &report::report_root()));
                }
                ReportType::Cli => {
                    // cli: grouped by file
                    let mut current = "";
                    for d in &diags {
                        if &*d.file != current {
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
                            report_type.as_str(),
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
                    status(
                        format!(
                            "Wrote {} diagnostic hashes to {}",
                            hashes.len(),
                            p.display()
                        ),
                        report_owns_stdout(report_type, output_file.as_ref()),
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
        Commands::Loc {
            directory,
            config,
            report_type,
            output_file,
            ignore_hashes,
            output_hashes,
            min_severity,
            ignore_codes,
            only_codes,
            allow_empty,
        } => {
            use cwtools_localization::{LocService, validate_loc_project};

            let file_cfg = load_config(config.as_deref(), directory.as_deref());
            let mut applied: Vec<&'static str> = Vec::new();
            let fc = file_cfg.as_ref();
            let directory = config::pick(
                directory,
                fc.and_then(|c| c.directory.clone()),
                "directory",
                &mut applied,
            );
            let report_type = config::pick(
                report_type,
                fc.and_then(|c| c.report_type),
                "report-type",
                &mut applied,
            )
            .unwrap_or(ReportType::Cli);
            let min_severity = config::pick(
                min_severity,
                fc.and_then(|c| c.min_severity),
                "min-severity",
                &mut applied,
            );
            let ignore_codes = config::pick_list(
                ignore_codes,
                fc.map(|c| c.ignore_codes.clone()).unwrap_or_default(),
                "ignore-codes",
                &mut applied,
            );
            let only_codes = config::pick_list(
                only_codes,
                fc.map(|c| c.only_codes.clone()).unwrap_or_default(),
                "only-codes",
                &mut applied,
            );
            let allow_empty = config::pick_flag(
                allow_empty,
                fc.is_some_and(|c| c.allow_empty),
                "allow-empty",
                &mut applied,
            );
            announce_config("loc", fc, &applied, config::LOC_KEYS);
            let directory = directory
                .unwrap_or_else(|| missing_required("loc", "<DIRECTORY>", "directory", fc));

            // A path that doesn't resolve is never a clean run, and --allow-empty
            // doesn't excuse it (that flag covers a deliberately empty scan).
            if !directory.is_dir() {
                eprintln!(
                    "error: directory does not exist: {}",
                    resolved_path(&directory)
                );
                std::process::exit(EXIT_DISCOVERY_FAILED);
            }

            let divert = report_owns_stdout(report_type, output_file.as_ref());
            status(
                format!("Scanning localisation in {}", directory.display()),
                divert,
            );
            let service = LocService::from_folder(&directory);
            exit_if_empty(
                service.files().len(),
                allow_empty,
                "no localisation files found under",
                &directory,
            );

            let total_entries: usize = service.files().iter().map(|f| f.entries.len()).sum();

            // Load the ignore-hash baseline, if given. Same placement as
            // `validate`: diagnostics are dropped before the report is
            // rendered and before they're counted for the exit code.
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

            // Standalone loc lint uses the scope-independent checks (CW225 etc.);
            // scope-aware command checks need the referencing config's scope.
            let want_legacy_hash = !ignored.is_empty();
            let mut sources = SourceLines::default();
            let keep = |d: &Diag| {
                !is_ignored(&ignored, d)
                    && min_severity.is_none_or(|m| severity_rank(d.severity) >= severity_rank(m))
            };
            let diags: Vec<Diag> = validate_loc_project(&service)
                .into_iter()
                .filter(|d| codes::wanted(d.code, &only_codes, &ignore_codes))
                .map(|d| {
                    let line_text = sources.trimmed(&d.file, d.line as u32).to_string();
                    loc_diagnostic_to_diag(&directory, d, &line_text, want_legacy_hash)
                })
                .filter(keep)
                .collect();

            // Surface parse failures too (files that couldn't even be
            // lenient-parsed), kept separate from `diags` since they carry no
            // line/code and get their own text-report line below.
            // Neither code filter touches these: they carry no code to name, and
            // dropping a file the parser couldn't read at all would exit 0 on a
            // broken mod. Suppression is per code; these have none.
            let parse_errors: Vec<Diag> = service
                .errors()
                .iter()
                .map(|(file, message)| {
                    loc_parse_error_to_diag(
                        &directory,
                        file.clone(),
                        message.clone(),
                        want_legacy_hash,
                    )
                })
                .filter(keep)
                .collect();

            let total_issues = diags.len() + parse_errors.len();
            // Severity-aware like `validate`: a parse failure is always an
            // error; a lint diagnostic only counts if it's Error-severity, so
            // e.g. Information-severity CW234 placeholders don't fail CI.
            let total_errors = diags
                .iter()
                .filter(|d| d.severity == ErrorSeverity::Error)
                .count()
                + parse_errors.len();

            // Render the report in the requested format. The `cli` default
            // reproduces the original hand-rolled text report byte-for-byte;
            // csv/json reuse the same row helpers `validate` uses.
            let mut out = String::new();
            match report_type {
                ReportType::Csv => {
                    out.push_str("file,line,severity,code,message,hash\n");
                    for d in diags.iter().chain(parse_errors.iter()) {
                        out.push_str(&csv_row(d));
                    }
                }
                ReportType::Json => {
                    let all: Vec<&Diag> = diags.iter().chain(parse_errors.iter()).collect();
                    out.push_str("[\n");
                    for (i, d) in all.iter().enumerate() {
                        out.push_str(&json_row(d, i + 1 >= all.len()));
                    }
                    out.push_str("]\n");
                }
                ReportType::Github => {
                    let root = report::report_root();
                    for d in diags.iter().chain(parse_errors.iter()) {
                        out.push_str(&report::github_row(d, &root));
                    }
                }
                ReportType::Sarif => {
                    let all: Vec<&Diag> = diags.iter().chain(parse_errors.iter()).collect();
                    out.push_str(&report::sarif_report(&all, &report::report_root()));
                }
                ReportType::Cli => {
                    let mut by_file: std::collections::BTreeMap<&str, Vec<&Diag>> =
                        std::collections::BTreeMap::new();
                    for d in &diags {
                        by_file.entry(&d.file).or_default().push(d);
                    }
                    for (file, ds) in &by_file {
                        out.push_str(&format!("\n  {} — {} issues:\n", file, ds.len()));
                        for d in ds {
                            out.push_str(&format!(
                                "    [line {}] {}: {}\n",
                                d.line, d.code, d.message
                            ));
                        }
                    }
                    for d in &parse_errors {
                        out.push_str(&format!("\n  {} — PARSE ERROR: {}\n", d.file, d.message));
                    }
                    out.push_str(&format!(
                        "\nLoc validation complete: {} entries, {} issues\n",
                        total_entries, total_issues
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
                            "Wrote {} report ({} issues) to {}",
                            report_type.as_str(),
                            total_issues,
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
                let mut hashes: Vec<&str> = diags
                    .iter()
                    .chain(parse_errors.iter())
                    .map(|d| d.hash.as_str())
                    .collect();
                hashes.sort_unstable();
                hashes.dedup();
                if let Err(e) = std::fs::write(p, hashes.join("\n")) {
                    eprintln!("Error writing hashes {}: {}", p.display(), e);
                } else {
                    status(
                        format!(
                            "Wrote {} diagnostic hashes to {}",
                            hashes.len(),
                            p.display()
                        ),
                        report_owns_stdout(report_type, output_file.as_ref()),
                    );
                }
            }

            let code = exit_code(total_errors, false, write_failed);
            if code != 0 {
                std::process::exit(code);
            }
        }
        Commands::Fix {
            config,
            game,
            directory,
            rules,
            vanilla,
            vanilla_cache,
            no_vanilla_cache,
            refresh_vanilla_cache,
            ignore_files,
            ignore_dirs,
            loc_language,
            case_sensitive_files,
            codes: only_flag,
            apply,
            allow_empty,
        } => {
            use cwtools_driver::{RulesInput, Session, SessionConfig, VanillaCacheAuto};
            use cwtools_game::constants::Game;
            use std::collections::BTreeMap;

            let file_cfg = load_config(config.as_deref(), directory.as_deref());
            let mut applied: Vec<&'static str> = Vec::new();
            let fc = file_cfg.as_ref();
            let game = config::pick(game, fc.and_then(|c| c.game.clone()), "game", &mut applied);
            let directory = config::pick(
                directory,
                fc.and_then(|c| c.directory.clone()),
                "directory",
                &mut applied,
            );
            let rules = config::pick(
                rules,
                fc.and_then(|c| c.rules.clone()),
                "rules",
                &mut applied,
            );
            let vanilla = config::pick(
                vanilla,
                fc.and_then(|c| c.vanilla.clone()),
                "vanilla",
                &mut applied,
            );
            let vanilla_cache = config::pick(
                vanilla_cache,
                fc.and_then(|c| c.vanilla_cache.clone()),
                "vanilla-cache",
                &mut applied,
            );
            let no_vanilla_cache = config::pick_flag(
                no_vanilla_cache,
                fc.is_some_and(|c| c.no_vanilla_cache),
                "no-vanilla-cache",
                &mut applied,
            );
            let refresh_vanilla_cache = config::pick_flag(
                refresh_vanilla_cache,
                fc.is_some_and(|c| c.refresh_vanilla_cache),
                "refresh-vanilla-cache",
                &mut applied,
            );
            let case_sensitive_files = config::pick_flag(
                case_sensitive_files,
                fc.is_some_and(|c| c.case_sensitive_files),
                "case-sensitive-files",
                &mut applied,
            );
            let ignore_files = config::pick_list(
                ignore_files,
                fc.map(|c| c.ignore_files.clone()).unwrap_or_default(),
                "ignore-files",
                &mut applied,
            );
            let ignore_dirs = config::pick_list(
                ignore_dirs,
                fc.map(|c| c.ignore_dirs.clone()).unwrap_or_default(),
                "ignore-dirs",
                &mut applied,
            );
            let loc_language = config::pick_list(
                loc_language,
                fc.map(|c| c.loc_languages.clone()).unwrap_or_default(),
                "loc-languages",
                &mut applied,
            );
            // `--code` is `fix`'s own spelling of the config's `only-codes`. It
            // predates the validated code flags and stays lenient: an unknown
            // code warns rather than failing the run.
            let only_flag: Vec<String> = only_flag
                .iter()
                .map(|c| c.to_ascii_uppercase())
                .inspect(|c| {
                    if codes::entry(c).is_none() {
                        eprintln!("warn: --code {c} is not a code the validator emits");
                    }
                })
                .collect();
            let only_codes = config::pick_list(
                only_flag,
                fc.map(|c| c.only_codes.clone()).unwrap_or_default(),
                "only-codes",
                &mut applied,
            );
            let ignore_codes = config::pick_list(
                Vec::new(),
                fc.map(|c| c.ignore_codes.clone()).unwrap_or_default(),
                "ignore-codes",
                &mut applied,
            );
            let allow_empty = config::pick_flag(
                allow_empty,
                fc.is_some_and(|c| c.allow_empty),
                "allow-empty",
                &mut applied,
            );
            announce_config("fix", fc, &applied, config::FIX_KEYS);

            let game = game.unwrap_or_else(|| missing_required("fix", "--game <GAME>", "game", fc));
            let directory = directory.unwrap_or_else(|| {
                missing_required("fix", "--directory <DIRECTORY>", "directory", fc)
            });
            let rules =
                rules.unwrap_or_else(|| missing_required("fix", "--rules <RULES>", "rules", fc));

            let game_id = Game::from_str(&game).unwrap_or_else(|| {
                eprintln!("Unknown game: {}. Supported: hoi4, stellaris, eu4, ck2, ck3, vic2, vic3, ir, eu5, custom", game);
                std::process::exit(1);
            });

            let want = |code: &str| codes::wanted(code, &only_codes, &ignore_codes);

            let vanilla_cache_index = vanilla_cache
                .as_ref()
                .and_then(|p| vanilla_cache::load(p).ok())
                .map(|(_, fp, data)| (fp, data));
            let (_fp, vanilla_cache_index) = vanilla_cache_index.unzip();

            // Same automatic base-game cache as `validate`, so both commands see
            // the same base-game data (and share the warm cache).
            let vanilla_cache_auto = if no_vanilla_cache || vanilla_cache.is_some() {
                None
            } else {
                cwtools_driver::default_cache_dir().map(|dir| VanillaCacheAuto {
                    dir,
                    refresh: refresh_vanilla_cache,
                })
            };

            let session = Session::load_with_parse_cache(
                SessionConfig {
                    game: game_id,
                    rules: RulesInput::from_path(rules.clone()),
                    directory: directory.clone(),
                    vanilla: vanilla.clone(),
                    vanilla_cache: vanilla_cache_index,
                    vanilla_cache_auto,
                    ignore_files: &ignore_files,
                    ignore_dirs: &ignore_dirs,
                    loc_languages: if loc_language.is_empty() {
                        None
                    } else {
                        Some(loc_language)
                    },
                    case_sensitive_files,
                    on_rules_warning: Some(&mut |w: String| eprintln!("warn: {}", w)),
                },
                cwtools_driver::default_cache_dir(),
            );

            // Same guards as `validate`: a failed walk, an empty ruleset, or an
            // empty file set must not read as "nothing needed fixing".
            if session.discovery_failed {
                std::process::exit(EXIT_DISCOVERY_FAILED);
            }
            exit_if_empty(
                session.ruleset().types.len(),
                allow_empty,
                "--rules loaded 0 types",
                &rules,
            );
            exit_if_empty(
                session.parsed_files().len(),
                allow_empty,
                "--directory contains no files to fix",
                &directory,
            );

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
                                .push((code.to_string(), edit));
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
                        by_file
                            .entry(d.file.clone())
                            .or_default()
                            .push((d.code.to_string(), edit));
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
                let (kept, skipped) = cwtools_parser::fix::plan_file_edits(&text, planned);
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

    #[test]
    fn empty_input_error_names_the_input_and_resolved_path() {
        let msg = empty_input_error("--rules loaded 0 types", std::path::Path::new("."));
        assert!(msg.contains("--rules loaded 0 types"), "got: {msg}");
        assert!(msg.contains("--allow-empty"), "got: {msg}");
        // The path is absolutized, so a relative CI path is identifiable.
        let here = std::path::absolute(".").unwrap().display().to_string();
        assert!(msg.contains(&here), "got: {msg}");
    }

    fn err_base() -> ValidationError {
        ValidationError {
            message: "redundant default, remove it".to_string(),
            severity: ErrorSeverity::Information,
            line: 12,
            col: 4,
            file: "common/ideas/x.txt".into(),
            code: Some("CW282"),
            fix: None,
            end: None,
        }
    }

    // ── Diagnostic hashes ────────────────────────────────────────────────────

    /// Bug: the digest used to be keyed on whatever path string an invocation
    /// happened to produce. The same file under the mod root, spelled
    /// absolute in one run, relative in another, `./`-prefixed in a third and
    /// backslash-separated in a fourth (as a Windows run would spell it),
    /// must all collapse to one digest.
    #[test]
    fn diag_hash_is_stable_across_path_spellings_of_the_same_file() {
        let root = Path::new("/repo/mod");
        let spellings = [
            "/repo/mod/common/x.txt",
            "common/x.txt",
            "/repo/mod/./common/x.txt",
            r"\repo\mod\common\x.txt",
        ];
        let hashes: Vec<String> = spellings
            .iter()
            .map(|f| diag_hash(root, f, "CW282", "m", "cost = 150"))
            .collect();
        for (spelling, hash) in spellings.iter().zip(&hashes) {
            assert_eq!(
                *hash, hashes[0],
                "{spelling:?} must hash the same as {:?}",
                spellings[0]
            );
        }
    }

    /// A file outside the root (e.g. a vanilla install path reported
    /// alongside mod files) still produces a stable digest, not a panic.
    #[test]
    fn diag_hash_is_stable_for_a_file_outside_the_root() {
        let root = Path::new("/repo/mod");
        let a = diag_hash(root, "/vanilla/common/x.txt", "CW282", "m", "cost = 150");
        let b = diag_hash(root, "/vanilla/common/x.txt", "CW282", "m", "cost = 150");
        assert_eq!(a, b);
    }

    /// `diag_hash` strips the mod root, regardless of how the file spelling
    /// and the root spelling disagree.
    #[test]
    fn relative_file_strips_the_root_regardless_of_spelling() {
        let root = Path::new("/repo/mod");
        let want = "common/x.txt";
        assert_eq!(relative_file("/repo/mod/common/x.txt", root), want);
        assert_eq!(
            relative_file("common/x.txt", root),
            want,
            "already relative"
        );
        assert_eq!(
            relative_file("/repo/mod/./common/x.txt", root),
            want,
            "a `./` component"
        );
        assert_eq!(
            relative_file("/repo/mod/common/x.txt", Path::new("/repo/mod/")),
            want,
            "trailing separator on the root"
        );
        assert_eq!(
            relative_file(r"\repo\mod\common\x.txt", root),
            want,
            "backslash-separated spelling"
        );
    }

    /// A file genuinely outside the root (e.g. a vanilla install path reported
    /// alongside mod files) falls back to the file string rather than panicking,
    /// and does so the same way every time.
    #[test]
    fn relative_file_falls_back_and_does_not_panic_when_outside_the_root() {
        let root = Path::new("/repo/mod");
        let outside = "/vanilla/common/x.txt";
        assert_eq!(relative_file(outside, root), outside);
        assert_eq!(relative_file(outside, root), relative_file(outside, root));
    }

    /// Same diagnostic on the same source text, moved down two lines.
    #[test]
    fn hash_survives_line_motion() {
        let mut moved = err_base();
        moved.line += 2;
        let root = Path::new(".");
        let before = validation_to_diag(root, err_base(), "cost = 150", true);
        let after = validation_to_diag(root, moved, "cost = 150", true);
        assert_eq!(
            before.hash, after.hash,
            "inserting a line above a diagnostic must not change its digest"
        );
        assert_ne!(
            before.legacy_hash, after.legacy_hash,
            "the legacy digest is the one that moved"
        );
    }

    /// Editing the offending line is a real change: the baseline entry should
    /// stop matching so the diagnostic is re-triaged.
    #[test]
    fn hash_changes_when_the_source_line_changes() {
        let root = Path::new(".");
        let a = validation_to_diag(root, err_base(), "cost = 150", true);
        let b = validation_to_diag(root, err_base(), "cost = 200", true);
        assert_ne!(a.hash, b.hash);
    }

    /// Migration: a baseline written with the old line-number digest still
    /// suppresses its diagnostic, and only the new digest is emitted.
    #[test]
    fn legacy_hashes_still_match_but_are_not_emitted() {
        let root = Path::new(".");
        let d = validation_to_diag(root, err_base(), "cost = 150", true);
        let legacy = legacy_diag_hash(
            root,
            "common/ideas/x.txt",
            "CW282",
            "redundant default, remove it",
            12,
        );
        assert_eq!(d.legacy_hash, legacy);
        assert_ne!(
            d.hash, legacy,
            "the emitted digest is the content-derived one"
        );

        let baseline: std::collections::HashSet<String> = [legacy].into_iter().collect();
        assert!(
            is_ignored(&baseline, &d),
            "old baselines must keep matching"
        );

        let fresh: std::collections::HashSet<String> = [d.hash.clone()].into_iter().collect();
        assert!(is_ignored(&fresh, &d), "new baselines match too");
        // The report and --output-hashes only ever see the new digest.
        assert!(csv_row(&d).contains(&d.hash));
        assert!(!csv_row(&d).contains(&d.legacy_hash));
    }

    /// The legacy digest exists only to match an `--ignore-hashes` baseline, so
    /// a run without one does not compute it. Everything else about the row is
    /// unchanged, including the emitted digest.
    #[test]
    fn legacy_hash_is_skipped_without_a_baseline() {
        let root = Path::new(".");
        let with_baseline = validation_to_diag(root, err_base(), "cost = 150", true);
        let without = validation_to_diag(root, err_base(), "cost = 150", false);

        assert!(
            without.legacy_hash.is_empty(),
            "no baseline means no legacy digest, got {:?}",
            without.legacy_hash
        );
        assert!(!with_baseline.legacy_hash.is_empty());
        assert_eq!(
            with_baseline.hash, without.hash,
            "the emitted digest must not depend on whether a baseline was loaded"
        );
        assert_eq!(csv_row(&with_baseline), csv_row(&without));
    }

    /// Skipping the legacy digest must not change which diagnostics a baseline
    /// suppresses. The new digest still matches, and an unrelated baseline still
    /// does not — an empty `legacy_hash` must never be treated as a match.
    #[test]
    fn skipping_the_legacy_hash_does_not_change_suppression() {
        let root = Path::new(".");
        let d = validation_to_diag(root, err_base(), "cost = 150", false);

        let fresh: std::collections::HashSet<String> = [d.hash.clone()].into_iter().collect();
        assert!(
            is_ignored(&fresh, &d),
            "a baseline holding the new digest still suppresses"
        );

        let unrelated: std::collections::HashSet<String> =
            ["0000000000000000".to_string()].into_iter().collect();
        assert!(
            !is_ignored(&unrelated, &d),
            "an unrelated baseline must not suppress"
        );
        assert!(
            !is_ignored(&std::collections::HashSet::new(), &d),
            "an empty baseline must not suppress"
        );
    }

    /// The legacy digest is a compatibility contract with baselines already on
    /// disk, so its bytes are frozen, not just its shape. This value is FNV-1a-64
    /// over `common/ideas/x.txt|CW282|redundant default|12`, exactly what the
    /// old line-number `diag_hash` emitted.
    #[test]
    fn legacy_hash_digest_is_frozen() {
        assert_eq!(
            legacy_diag_hash(
                Path::new("."),
                "common/ideas/x.txt",
                "CW282",
                "redundant default",
                12
            ),
            "8e7fd969bd9ea463"
        );
    }

    /// Whole-file diagnostics (line 0) have no source line; the digest still
    /// distinguishes them by file/code/message.
    #[test]
    fn parse_error_hashes_are_distinct_without_a_source_line() {
        let root = Path::new(".");
        let a = loc_parse_error_to_diag(root, "l_english.yml".into(), "bad yaml".into(), true);
        let b = loc_parse_error_to_diag(root, "l_english.yml".into(), "worse yaml".into(), true);
        assert_ne!(a.hash, b.hash);
    }

    #[test]
    fn source_lines_trims_and_handles_missing_input() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("x.txt");
        std::fs::write(&file, "first\n    indented = yes\n").unwrap();
        let path = file.to_str().unwrap();

        let mut sources = SourceLines::default();
        assert_eq!(sources.trimmed(path, 2), "indented = yes");
        assert_eq!(sources.trimmed(path, 1), "first");
        assert_eq!(sources.trimmed(path, 0), "", "line 0 has no source line");
        assert_eq!(sources.trimmed(path, 99), "", "past the end of the file");
        assert_eq!(sources.trimmed("/no/such/file.txt", 1), "");
        // The slot re-fills when the file changes back.
        assert_eq!(sources.trimmed(path, 1), "first");
    }

    // Inertness guard (Task 8/18, step 2): a fix payload AND an end position must
    // not change the report. The `Diag` mapping and every report row read neither —
    // locked in here so populating the emit sites keeps validate output byte-identical.
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
        // Task 18: the end position is inert in the report too.
        with_fix.end = Some((12, 30));

        let root = Path::new(".");
        let d0 = validation_to_diag(root, base, "cost = 150", true);
        let d1 = validation_to_diag(root, with_fix, "cost = 150", true);

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
        let forward: Vec<PlannedFix> = vec![
            ("CWA".into(), edit(1, 0, 1, 4, "X")),
            ("CWB".into(), edit(1, 5, 1, 9, "Y")),
        ];
        let reversed: Vec<PlannedFix> = vec![
            ("CWB".into(), edit(1, 5, 1, 9, "Y")),
            ("CWA".into(), edit(1, 0, 1, 4, "X")),
        ];
        for planned in [forward, reversed] {
            let (kept, skipped) = cwtools_parser::fix::plan_file_edits(text, planned);
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
        let planned: Vec<PlannedFix> = vec![
            ("CWA".into(), edit(1, 0, 1, 6, "X")), // covers "aaaa b"
            ("CWB".into(), edit(1, 5, 1, 9, "Y")), // overlaps at col 5
        ];
        let (kept, skipped) = cwtools_parser::fix::plan_file_edits(text, planned);
        assert_eq!(kept.len(), 1, "one edit kept");
        assert_eq!(skipped, vec!["CWB".to_string()], "overlapping edit skipped");
        assert_eq!(cwtools_parser::fix::apply_edits(text, &kept), "Xbbb\n");
    }
}
