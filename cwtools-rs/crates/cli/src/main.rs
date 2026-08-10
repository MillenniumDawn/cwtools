use clap::Parser;

mod cli;
mod codes;
mod commands;
mod config;
mod diag;
mod report;
mod run;

fn main() {
    // Quiet by default; set RUST_LOG or CWTOOLS_PROFILE to turn on logging /
    // profiling. See PROFILING.md and `cwtools_profiling`.
    cwtools_profiling::init_tracing();
    commands::dispatch(cli::Cli::parse().command);
}
