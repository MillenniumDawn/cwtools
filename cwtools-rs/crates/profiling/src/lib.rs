//! Shared profiling + logging helpers for the CLI and LSP binaries.
//!
//! Two knobs, both off by default so a normal run stays quiet:
//!
//! - `RUST_LOG`: standard env-filter (e.g. `RUST_LOG=cwtools_validation=debug`).
//! - `CWTOOLS_PROFILE`: turn on the profiling report. Span timings at `info`
//!   plus RSS samples at phase boundaries (see [`log_rss`]).
//!
//! Both write to **stderr** so they never corrupt the LSP's stdout JSON-RPC
//! channel. Instrument a hot path with `#[tracing::instrument(skip_all)]` to
//! have it timed; see PROFILING.md.

use tracing_subscriber::fmt::format::FmtSpan;

/// True when `CWTOOLS_PROFILE` is set to a truthy value (`1`, `true`, `yes`, `on`).
pub fn profile_enabled() -> bool {
    matches!(
        std::env::var("CWTOOLS_PROFILE").ok().as_deref(),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

/// Install the global tracing subscriber when `RUST_LOG` or `CWTOOLS_PROFILE`
/// is set. Idempotent and safe to call from every binary's `main` — a second
/// call (or a competing subscriber) is ignored. Always writes to stderr.
pub fn init_tracing() {
    let rust_log = std::env::var("RUST_LOG").ok();
    if rust_log.is_none() && !profile_enabled() {
        return;
    }

    let filter = match rust_log {
        Some(_) => tracing_subscriber::EnvFilter::from_default_env(),
        // CWTOOLS_PROFILE on its own implies "show me the timings".
        None => tracing_subscriber::EnvFilter::new("info"),
    };

    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_writer(std::io::stderr)
        // Emit span-close events so instrumented hot paths report busy/idle
        // time — that's the profiling signal.
        .with_span_events(FmtSpan::CLOSE)
        .try_init();
}

/// Current resident set size (physical memory) of this process, in bytes.
/// Linux-only (reads `/proc/self/status`); returns `None` elsewhere or on error.
pub fn current_rss_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            // Format: "VmRSS:\t   123456 kB"
            let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kb * 1024);
        }
    }
    None
}

/// Format a byte count as a human-readable MiB string, e.g. `1462.3 MiB`.
pub fn format_mib(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
}

/// Return freed heap back to the OS (glibc `malloc_trim`). After a big transient
/// (the workspace scan parses the whole base game + ~2M loc entries, then drops
/// them) glibc holds the freed pages, so RSS stays at the peak. Calling this once
/// the transients are gone drops RSS toward the real working set. No-op off glibc.
pub fn trim_memory() {
    #[cfg(target_os = "linux")]
    // SAFETY: malloc_trim takes no ownership and is safe to call at any time;
    // it only releases free heap. The return value (1 = memory freed) is ignored.
    unsafe {
        libc::malloc_trim(0);
    }
}

/// When profiling is enabled, log the current RSS at a named phase boundary.
/// Cheap no-op otherwise. Use this around the expensive phases (workspace
/// scan, loc rebuild) to see where memory is spent and whether it is released.
pub fn log_rss(phase: &str) {
    if !profile_enabled() {
        return;
    }
    match current_rss_bytes() {
        Some(bytes) => {
            tracing::info!(target: "cwtools::profile", phase, rss = %format_mib(bytes), "rss sample")
        }
        None => {
            tracing::info!(target: "cwtools::profile", phase, "rss sample (unavailable on this platform)")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_mib_rounds() {
        assert_eq!(format_mib(1024 * 1024), "1.0 MiB");
        assert_eq!(format_mib(1536 * 1024), "1.5 MiB");
    }

    #[test]
    fn rss_is_positive_on_linux() {
        // On Linux this process has a non-zero RSS; elsewhere None is fine.
        if let Some(bytes) = current_rss_bytes() {
            assert!(bytes > 0);
        }
    }
}
