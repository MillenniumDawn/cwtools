//! One module per subcommand, behind a dispatcher. Each `run` owns its
//! subcommand's argument resolution, execution and exit behavior; anything two
//! of them share lives in `crate::run`, `crate::diag` or `crate::config`.

use crate::cli::Commands;

mod cache;
mod discover;
mod fix;
mod loc;
mod parse;
mod rules;
mod validate;

/// Route a parsed subcommand to its module.
pub(crate) fn dispatch(command: Commands) {
    match command {
        Commands::Parse { file } => parse::run(file),
        Commands::Discover { directory } => discover::run(directory),
        Commands::Serialize { input, output } => cache::serialize(input, output),
        Commands::Deserialize { input } => cache::deserialize(input),
        Commands::Rules { file } => rules::run(file),
        Commands::Validate(args) => validate::run(args),
        Commands::CacheVanilla {
            game,
            vanilla,
            rules,
            output,
        } => cache::vanilla(game, vanilla, rules, output),
        Commands::Loc(args) => loc::run(args),
        Commands::Fix(args) => fix::run(args),
    }
}
