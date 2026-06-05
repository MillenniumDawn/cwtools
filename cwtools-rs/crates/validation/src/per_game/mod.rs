use crate::ValidationError;
use cwtools_game::constants::Game;
use cwtools_parser::ast::ParsedFile;
use cwtools_rules::rules_types::RuleSet;
use cwtools_string_table::string_table::StringTable;

pub mod common;
pub mod eu4;
pub mod stellaris;

/// Run game-specific validators after generic rule validation.
pub fn run_game_validators(
    ast: &ParsedFile,
    ruleset: &RuleSet,
    table: &StringTable,
    file_path: &str,
    game: Game,
) -> Vec<ValidationError> {
    let mut errors = Vec::new();

    // Common checks (unique types, should_be_referenced, warning_only downgrade)
    common::validate_common(ast, ruleset, table, file_path, &mut errors);

    match game {
        Game::Stellaris => {
            stellaris::validate_stellaris(ast, ruleset, table, file_path, &mut errors);
        }
        Game::Eu4 => {
            eu4::validate_eu4(ast, ruleset, table, file_path, &mut errors);
        }
        _ => {
            // Other games: only common checks for now
        }
    }

    errors
}
