//! Plumbing every subcommand shares: exit codes, `cwtools.toml` resolution and
//! reporting, and the status lines that have to keep out of a redirected report.

use clap::CommandFactory;
use cwtools_game::constants::Game;
use std::path::{Path, PathBuf};

use crate::cli::Cli;
use crate::config;
use crate::report::ReportType;

/// The file walk itself failed (the path doesn't resolve, a dir is unreadable).
pub(crate) const EXIT_DISCOVERY_FAILED: i32 = 3;

/// An input resolved to nothing: a ruleset with no types, or a target directory
/// with no files. Distinct from [`EXIT_DISCOVERY_FAILED`] so CI can tell
/// "nothing to check" from "the walk errored".
pub(crate) const EXIT_EMPTY_INPUT: i32 = 4;

/// An input the run couldn't act on: a `cwtools.toml` that wouldn't read or
/// parse, a `--since` ref git couldn't resolve. Shares clap's usage-error code,
/// since the run never started and so has no validation result to report.
pub(crate) const EXIT_USAGE: i32 = 2;

/// Resolve the run's config file, failing loudly on a broken one. `anchor` is
/// the directory the upward search starts from when `--config` wasn't given.
pub(crate) fn load_config(
    explicit: Option<&Path>,
    anchor: Option<&Path>,
) -> Option<config::FileConfig> {
    config::resolve(explicit, anchor).unwrap_or_else(|e| {
        eprintln!("error: {e}");
        std::process::exit(EXIT_USAGE);
    })
}

/// Whether stdout is carrying one of the CI report formats, in which case status
/// lines have to go to stderr instead: `cwtools loc . --report-type sarif >
/// out.sarif` must not have a progress banner in the middle of the JSON. Only
/// the two new formats divert — cli, csv and json keep every line where it was.
pub(crate) fn report_owns_stdout(report_type: ReportType, output_file: Option<&PathBuf>) -> bool {
    output_file.is_none() && matches!(report_type, ReportType::Github | ReportType::Sarif)
}

/// A progress/status line, diverted to stderr when the report owns stdout.
pub(crate) fn status(line: String, to_stderr: bool) {
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
pub(crate) fn announce_config(
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

/// The run-level notice for a validate that loaded no base-game index, or
/// `None` when it did. A report is read as "nothing wrong here", so the checks
/// that could not run have to be named rather than left to look clean.
pub(crate) fn vanilla_notice(game: Game, has_vanilla: bool) -> Option<String> {
    let codes = cwtools_driver::vanilla_gated_checks(game, has_vanilla);
    if codes.is_empty() {
        return None;
    }
    Some(format!(
        "no base-game data loaded, so {} report nothing; pass --vanilla or --vanilla-cache to run them",
        codes.join(", ")
    ))
}

/// Bail on a setting that neither a flag nor the config file supplied, through
/// clap so the message, the usage line and the exit code match every other
/// usage error.
pub(crate) fn missing_required(
    subcommand: &str,
    arg: &str,
    key: &str,
    cfg: Option<&config::FileConfig>,
) -> ! {
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
pub(crate) fn exit_code(total_errors: usize, discovery_failed: bool, write_failed: bool) -> i32 {
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
pub(crate) fn resolved_path(path: &std::path::Path) -> String {
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
pub(crate) fn exit_if_empty(count: usize, allow_empty: bool, what: &str, path: &std::path::Path) {
    if count == 0 && !allow_empty {
        eprintln!("{}", empty_input_error(what, path));
        std::process::exit(EXIT_EMPTY_INPUT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn vanilla_notice_is_silent_with_a_base_game_index() {
        assert_eq!(vanilla_notice(Game::Hoi4, true), None);
        assert_eq!(vanilla_notice(Game::Stellaris, true), None);
    }

    #[test]
    fn vanilla_notice_names_the_disabled_checks_and_the_flags() {
        let msg = vanilla_notice(Game::Hoi4, false).expect("notice without vanilla data");
        assert!(msg.contains("CW113, CW222, CW500"), "got: {msg}");
        assert!(msg.contains("--vanilla-cache"), "got: {msg}");
        // Stellaris adds the ship-design and planet-killer families.
        let stl = vanilla_notice(Game::Stellaris, false).expect("notice without vanilla data");
        assert!(stl.contains("CW227, CW229"), "got: {stl}");
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
}
