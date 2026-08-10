//! `loc`: the standalone localisation lint over a directory of `.yml` files.

use cwtools_file_manager::file_manager::ScanBudget;
use cwtools_localization::{LocService, validate_loc_project};
use cwtools_validation::ErrorSeverity;

use crate::cli::LocArgs;
use crate::diag::{
    Diag, SourceLines, csv_row, is_ignored, json_row, loc_diagnostic_to_diag,
    loc_parse_error_to_diag, severity_rank,
};
use crate::report::ReportType;
use crate::run::{
    EXIT_DISCOVERY_FAILED, announce_config, exit_code, exit_if_empty, load_config,
    missing_required, report_owns_stdout, resolved_path, status,
};
use crate::{codes, config, report};

pub(super) fn run(args: LocArgs) {
    let LocArgs {
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
    } = args;

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
    let directory =
        directory.unwrap_or_else(|| missing_required("loc", "<DIRECTORY>", "directory", fc));

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
    let service = LocService::from_folder(&directory, ScanBudget::default());
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
            loc_parse_error_to_diag(&directory, file.clone(), message.clone(), want_legacy_hash)
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
