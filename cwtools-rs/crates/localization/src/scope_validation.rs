//! Scope-aware localisation command validation.
//!
//! Validates chains like `[THIS.Owner.GetName]` by folding through the game's
//! `ScopeContext`.  Emits `LocCommandWrongScope` or `LocCommandChainEndsInScope`
//! when a chain is invalid.  Unknown commands are accepted leniently so missing
//! entries don't produce false positives.

use crate::commands::{Game, JominiCommand, LocEntry};
use cwtools_game::constants::Game as EngineGame;
use cwtools_game::scope_engine::{SCOPE_ANY, ScopeContext, ScopeId, ScopeResult};
use cwtools_game::scope_registry::ScopeRegistry;
use std::collections::HashSet;
use std::sync::Arc;

// ── Public types ──────────────────────────────────────────────────────────────

/// A diagnostic produced by `validate_loc_commands`.
#[derive(Debug, Clone, PartialEq)]
pub enum LocCommandDiagnostic {
    /// A scope-change link was used from an incompatible scope.
    ///
    /// Mirrors F# `LocContextResult.WrongScope`.
    WrongScope {
        /// The command segment that triggered the error.
        command: String,
        /// Numeric ID of the current scope at the point of failure.
        current_scope: u32,
        /// Numeric IDs of the scopes the command is valid in.
        expected_scopes: Vec<u32>,
    },
    /// The chain ended without reaching a terminal getter command.
    ///
    /// Mirrors F#'s "chain ends in scope rather than terminal command" check.
    ChainEndsInScope {
        /// Full command string that ended without a getter.
        command: String,
    },
    /// The command was not found in the scope registry at all.
    ///
    /// Mirrors F# `LocNotFound` / CW226 `InvalidLocCommand`.
    /// Only emitted when a scope registry is present (config-driven mode);
    /// without one the validator remains fully lenient to avoid false positives.
    NotFound {
        /// The unrecognised command segment.
        command: String,
    },
}

/// Per-game static data needed for loc-command validation.
///
/// The caller constructs this from their game configuration and passes it to
/// `validate_loc_commands`.  Using a struct keeps the function signature
/// stable while the data grows.
pub struct LocScopeData {
    /// Game variant (controls which scope links are loaded).
    pub game: Game,
    /// Terminal getter commands accepted for this game.
    ///
    /// If this is empty every unknown command is accepted (fully lenient).
    /// If non-empty, any unknown final segment not in this list will produce
    /// a `ChainEndsInScope` diagnostic.
    pub terminal_commands: Vec<String>,
    /// Whether `?variable` syntax is accepted (HOI4 / Stellaris).
    pub question_mark_variable: bool,
    /// Whether `parameter:xxx` references are accepted.
    pub parameter_variables: bool,
    /// Config-driven scope/link registry. When set, the loc scope engine uses it
    /// (shared with the validation path) instead of the hardcoded per-game table.
    pub registry: Option<Arc<ScopeRegistry>>,
}

impl Default for LocScopeData {
    fn default() -> Self {
        Self {
            game: Game::Generic,
            terminal_commands: Vec::new(),
            question_mark_variable: true,
            parameter_variables: true,
            registry: None,
        }
    }
}

/// Build the loc scope context: from the config registry when provided (shared
/// with the validation path), else from the game's hardcoded table.
fn build_loc_ctx(data: &LocScopeData, engine_game: EngineGame, initial: ScopeId) -> ScopeContext {
    match &data.registry {
        Some(reg) => ScopeContext::from_registry(reg.clone(), initial),
        None => ScopeContext::new(engine_game, initial),
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Validate all `[command]` and `[JominiCommand chain]` blocks in a loc entry.
///
/// * `entry`       — the parsed loc entry whose commands/jomini_commands to check.
/// * `initial_scope` — the scope context active where this loc string appears.
///   Pass `ScopeId(0)` (SCOPE_ANY) when the context is unknown.
/// * `data`        — per-game static settings.
///
/// Returns a (possibly empty) list of diagnostics.
pub fn validate_loc_commands(
    entry: &LocEntry,
    initial_scope: ScopeId,
    data: &LocScopeData,
) -> Vec<LocCommandDiagnostic> {
    // Nothing to validate: skip building the terminal set / engine mapping and
    // return the empty (non-allocating) Vec for the common no-command entry.
    if entry.commands.is_empty() && entry.jomini_commands.is_empty() {
        return Vec::new();
    }

    let engine_game = game_to_engine(data.game);
    let mut diags = Vec::new();

    // Lowercased terminal-command set, built once per entry. The membership test
    // (`is_terminal_command`) ran a linear case-insensitive scan per segment;
    // a set turns that into an O(1) lookup. Identifiers are ASCII.
    let terminal_set: HashSet<String> = data
        .terminal_commands
        .iter()
        .map(|c| c.to_ascii_lowercase())
        .collect();

    // Validate legacy [command] strings (single-segment, dot-split internally)
    for cmd in &entry.commands {
        validate_command_string(
            cmd,
            initial_scope,
            engine_game,
            data,
            &terminal_set,
            &mut diags,
        );
    }

    // Validate Jomini command chains — each inner Vec is one bracket's chain.
    for chain in &entry.jomini_commands {
        validate_jomini_chain(
            chain,
            initial_scope,
            engine_game,
            data,
            &terminal_set,
            &mut diags,
        );
    }

    diags
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Convert the localization crate's `Game` enum to the engine `Game` enum.
fn game_to_engine(game: Game) -> EngineGame {
    match game {
        Game::HOI4 => EngineGame::Hoi4,
        Game::Stellaris => EngineGame::Stellaris,
        Game::EU4 => EngineGame::Eu4,
        Game::CK3 => EngineGame::Ck3,
        Game::IR => EngineGame::Ir,
        Game::VIC3 => EngineGame::Vic3,
        Game::EU5 => EngineGame::Eu5,
        Game::Generic | Game::Custom => EngineGame::Hoi4, // fallback: lenient
    }
}

/// Returns true if `lower` (an already-lowercased command/segment) is a special
/// prefix that bypasses scope checks.
///
/// Mirrors F# handling of `event_target:`, `parameter:`, `?`. The caller
/// lowercases once and shares the result with `is_terminal_command`.
fn is_bypass_prefix(lower: &str, data: &LocScopeData) -> bool {
    lower.starts_with("event_target:")
        || lower.starts_with("scope:")
        || (data.parameter_variables && lower.starts_with("parameter:"))
        || (data.question_mark_variable && lower.starts_with('?'))
}

/// Validate a legacy dot-delimited command string, e.g. `THIS.Owner.GetName`.
fn validate_command_string(
    cmd: &str,
    initial_scope: ScopeId,
    engine_game: EngineGame,
    data: &LocScopeData,
    terminal_set: &HashSet<String>,
    diags: &mut Vec<LocCommandDiagnostic>,
) {
    if is_bypass_prefix(&cmd.to_ascii_lowercase(), data) {
        return;
    }

    let segments: Vec<&str> = cmd.split('.').collect();
    let last_idx = segments.len().saturating_sub(1);

    let mut ctx = build_loc_ctx(data, engine_game, initial_scope);

    for (i, seg) in segments.iter().enumerate() {
        let is_last = i == last_idx;

        // Lowercase once per segment; shared by the bypass and terminal checks.
        let seg_lower = seg.to_ascii_lowercase();

        // Bypass prefixes in any segment
        if is_bypass_prefix(&seg_lower, data) {
            ctx.push_scope(SCOPE_ANY);
            continue;
        }

        // Check if this looks like a terminal getter (starts with "Get" or
        // is in the explicit terminal-commands list)
        let looks_terminal = is_terminal_command(&seg_lower, terminal_set);

        if is_last && looks_terminal {
            // Terminal command — no scope check needed; accept.
            break;
        }

        // Attempt scope change via the engine
        let result = ctx.change_scope(seg);
        match result {
            ScopeResult::NewScope { .. } | ScopeResult::AnyScope | ScopeResult::VarFound => {
                // Scope changed successfully; continue the chain.
            }
            ScopeResult::ValueFound if is_last => {
                // Value-only trigger at the end: valid.
            }
            ScopeResult::ValueFound => {
                // Value-only trigger in the middle: chain cannot continue.
                // Treat as terminal (lenient — F# would error but we accept).
            }
            ScopeResult::WrongScope {
                command,
                current,
                expected,
            } => {
                diags.push(LocCommandDiagnostic::WrongScope {
                    command: format!("{} (in {})", command, cmd),
                    current_scope: current.0,
                    expected_scopes: expected.iter().map(|s| s.0).collect(),
                });
                // Short-circuit: further segments are meaningless
                return;
            }
            ScopeResult::NotFound | ScopeResult::VarNotFound(_) => {
                // Unknown command.  If it's the final segment and we have no
                // terminal-commands list, accept it (lenient); if we have a
                // non-empty list and it didn't match, warn.
                // NOTE: CW226 (NotFound) is NOT emitted here because this path
                // handles legacy `[command]` strings (single-segment, no dots).
                // F# only fires CW226 from validateJominiLocalisationCommandsBase,
                // not from validateLocalisationCommandsBase. Legacy commands like
                // `[var_name|fmt]` or `[2%%Y]` are valid HOI4 loc syntax and are
                // not scope links, so we remain lenient here.
                if is_last && !data.terminal_commands.is_empty() && !looks_terminal {
                    diags.push(LocCommandDiagnostic::ChainEndsInScope {
                        command: cmd.to_string(),
                    });
                } else if !is_last {
                    // Unknown intermediate — push ANY and continue leniently.
                    ctx.push_scope(SCOPE_ANY);
                }
            }
        }
    }
}

/// Validate a Jomini command chain (one `[...]` bracket's segments).
///
/// Scope is threaded through the segments left-to-right, mirroring
/// `validate_command_string`. This is the single scope-threading implementation
/// for Jomini chains; the old per-segment path is replaced.
fn validate_jomini_chain(
    chain: &[JominiCommand],
    initial_scope: ScopeId,
    engine_game: EngineGame,
    data: &LocScopeData,
    terminal_set: &HashSet<String>,
    diags: &mut Vec<LocCommandDiagnostic>,
) {
    if chain.is_empty() {
        return;
    }
    let last_idx = chain.len() - 1;
    let mut ctx = build_loc_ctx(data, engine_game, initial_scope);
    // Suppress CW226 when the chain has more than one segment. For chains like
    // [THIS.scripted_var] or [ROOT.var_name], the scope engine resolves the
    // intermediate (THIS/ROOT → NewScope) but the final segment is a scripted
    // variable or localisation reference that the Rust engine doesn't index.
    // F#'s HOI4 validator had the full scripted-variable registry; without it
    // we can only be confident about single-segment Jomini calls like [Func()].
    // Unknown intermediates (e.g. country-tag scopes like PAL) also poison the
    // chain, which is tracked below.
    let mut had_lenient_intermediate = chain.len() > 1;

    for (i, cmd) in chain.iter().enumerate() {
        let seg = &cmd.key;
        let is_last = i == last_idx;

        // Lowercase once per segment; shared by the bypass and terminal checks.
        let seg_lower = seg.to_ascii_lowercase();

        if is_bypass_prefix(&seg_lower, data) {
            ctx.push_scope(SCOPE_ANY);
            if !is_last {
                had_lenient_intermediate = true;
            }
            continue;
        }

        let looks_terminal = is_terminal_command(&seg_lower, terminal_set);
        if is_last && looks_terminal {
            return; // terminal — accepted without scope check
        }

        let result = ctx.change_scope(seg);
        match result {
            ScopeResult::AnyScope => {
                if !is_last {
                    had_lenient_intermediate = true;
                }
            }
            ScopeResult::NewScope { .. } | ScopeResult::VarFound => {}
            ScopeResult::ValueFound if is_last => return,
            ScopeResult::ValueFound => {}
            ScopeResult::WrongScope {
                command,
                current,
                expected,
            } => {
                diags.push(LocCommandDiagnostic::WrongScope {
                    command,
                    current_scope: current.0,
                    expected_scopes: expected.iter().map(|s| s.0).collect(),
                });
                return; // short-circuit
            }
            ScopeResult::NotFound | ScopeResult::VarNotFound(_) => {
                if is_last
                    && data.registry.is_some()
                    && !looks_terminal
                    && !had_lenient_intermediate
                {
                    // Registry present, chain resolved cleanly up to this point,
                    // final command is unknown: CW226 (mirrors F# `LocNotFound`).
                    diags.push(LocCommandDiagnostic::NotFound {
                        command: seg.to_string(),
                    });
                } else if !is_last {
                    ctx.push_scope(SCOPE_ANY); // unknown intermediate: lenient
                    had_lenient_intermediate = true;
                }
            }
        }
    }
}

/// Check if a command segment is (or looks like) a terminal getter.
///
/// Terminal commands end the chain and return a string/value — they don't
/// produce a new scope.
///
/// This covers the common Paradox naming convention (`GetName`, `GetDesc`,
/// `GetRuler`…) plus the per-game list provided in `LocScopeData`.
/// `lower` is the already-lowercased segment; `terminal_set` is the lowercased
/// terminal-command set (built once per entry by `validate_loc_commands`).
fn is_terminal_command(lower: &str, terminal_set: &HashSet<String>) -> bool {
    // Convention: terminal getters start with "Get" (case-insensitive)
    lower.starts_with("get") || terminal_set.contains(lower)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{Game, JominiCommand, LocEntry, Position};

    fn make_entry_with_commands(commands: Vec<String>) -> LocEntry {
        LocEntry {
            key: "test_key".into(),
            value: None,
            desc: "test".into(),
            position: Position::new(Arc::from("test.yml"), 1, 1),
            error_range: None,
            refs: Vec::new(),
            commands,
            jomini_commands: Vec::new(),
        }
    }

    fn make_entry_with_jomini(jomini: Vec<Vec<JominiCommand>>) -> LocEntry {
        LocEntry {
            key: "test_key".into(),
            value: None,
            desc: "test".into(),
            position: Position::new(Arc::from("test.yml"), 1, 1),
            error_range: None,
            refs: Vec::new(),
            commands: Vec::new(),
            jomini_commands: jomini,
        }
    }

    fn hoi4_data() -> LocScopeData {
        // HOI4 is config-driven: supply a minimal registry (country/state +
        // owner/controller links) so the scope chains resolve in tests.
        use cwtools_game::scope_engine::{ScopeId, ScopeLink};
        let mut reg = ScopeRegistry::default();
        for (name, id) in [("country", 100u32), ("state", 101u32)] {
            reg.by_name.insert(name.to_string(), ScopeId(id));
            reg.by_id.insert(
                ScopeId(id),
                cwtools_game::scope_registry::ScopeDefOwned {
                    name: name.to_string(),
                    aliases: vec![name.to_string()],
                    subscope_of: vec![],
                },
            );
        }
        for name in ["owner", "controller"] {
            reg.links.insert(
                name.to_string(),
                ScopeLink {
                    valid_scopes: vec![ScopeId(101)], // state only
                    target: Some(ScopeId(100)),       // -> country
                    is_scope_change: true,
                    ignore_keys: vec![],
                },
            );
        }
        LocScopeData {
            game: Game::HOI4,
            terminal_commands: vec![
                "GetName".into(),
                "GetNameDef".into(),
                "GetAdjective".into(),
                "GetLeader".into(),
            ],
            question_mark_variable: true,
            parameter_variables: true,
            registry: Some(Arc::new(reg)),
        }
    }

    // ── Valid chain: State → owner (Country) → GetName ────────────────────────

    #[test]
    fn valid_chain_state_owner_getname() {
        // Starting in HOI4 State (101): owner → Country (100) → GetName (terminal)
        let entry = make_entry_with_commands(vec!["owner.GetName".into()]);
        let data = hoi4_data();

        // Start in State scope (HOI4 State = 101)
        let diags = validate_loc_commands(&entry, ScopeId(101), &data);
        assert!(
            diags.is_empty(),
            "owner.GetName from State scope should be valid, got: {:?}",
            diags
        );
    }

    // ── Invalid chain: Country → controller (only valid from State) ───────────

    #[test]
    fn invalid_chain_country_controller_wrong_scope() {
        // Starting in HOI4 Country (100): `controller` is only valid from State (101)
        let entry = make_entry_with_commands(vec!["controller.GetName".into()]);
        let data = hoi4_data();

        let diags = validate_loc_commands(&entry, ScopeId(100), &data);
        assert!(
            !diags.is_empty(),
            "controller from Country scope should produce a WrongScope diagnostic"
        );
        assert!(
            matches!(diags[0], LocCommandDiagnostic::WrongScope { .. }),
            "expected WrongScope, got: {:?}",
            diags
        );
    }

    // ── Bypass: event_target: is always accepted ──────────────────────────────

    #[test]
    fn event_target_bypass() {
        let entry = make_entry_with_commands(vec!["event_target:my_target".into()]);
        let data = hoi4_data();
        let diags = validate_loc_commands(&entry, ScopeId(100), &data);
        assert!(diags.is_empty(), "event_target: should always be accepted");
    }

    // ── Bypass: parameter: is accepted ───────────────────────────────────────

    #[test]
    fn parameter_bypass() {
        let entry = make_entry_with_commands(vec!["parameter:my_param".into()]);
        let data = hoi4_data();
        let diags = validate_loc_commands(&entry, ScopeId(100), &data);
        assert!(diags.is_empty(), "parameter: should always be accepted");
    }

    // ── Bypass: ?variable is accepted ────────────────────────────────────────

    #[test]
    fn question_mark_variable_bypass() {
        let entry = make_entry_with_commands(vec!["?some_var".into()]);
        let data = hoi4_data();
        let diags = validate_loc_commands(&entry, ScopeId(100), &data);
        assert!(diags.is_empty(), "?variable should always be accepted");
    }

    // ── THIS/Root/PREV/FROM primary scopes ───────────────────────────────────

    #[test]
    fn primary_scope_this_getname() {
        let entry = make_entry_with_commands(vec!["THIS.GetName".into()]);
        let data = hoi4_data();
        let diags = validate_loc_commands(&entry, ScopeId(101), &data);
        assert!(diags.is_empty(), "THIS.GetName should always be valid");
    }

    #[test]
    fn primary_scope_root_getname() {
        let entry = make_entry_with_commands(vec!["Root.GetName".into()]);
        let data = hoi4_data();
        let diags = validate_loc_commands(&entry, ScopeId(101), &data);
        assert!(diags.is_empty(), "Root.GetName should always be valid");
    }

    // ── Jomini single-command GetName accepted ────────────────────────────────

    #[test]
    fn jomini_getname_accepted() {
        // A single bracket [GetName] — one chain with one segment
        let entry = make_entry_with_jomini(vec![vec![JominiCommand {
            key: "GetName".into(),
            params: Vec::new(),
        }]]);
        let data = hoi4_data();
        let diags = validate_loc_commands(&entry, ScopeId(100), &data);
        assert!(
            diags.is_empty(),
            "Jomini GetName should be accepted as terminal"
        );
    }

    // ── Jomini wrong-scope link produces diagnostic ───────────────────────────

    #[test]
    fn jomini_wrong_scope_controller_from_country() {
        // `controller` is only valid from State (101), not Country (100)
        let entry = make_entry_with_jomini(vec![vec![JominiCommand {
            key: "controller".into(),
            params: Vec::new(),
        }]]);
        let data = hoi4_data();
        let diags = validate_loc_commands(&entry, ScopeId(100), &data);
        assert!(
            !diags.is_empty(),
            "Jomini controller from Country should produce WrongScope"
        );
        assert!(
            matches!(diags[0], LocCommandDiagnostic::WrongScope { .. }),
            "expected WrongScope, got: {:?}",
            diags
        );
    }

    // ── Jomini dot-chain threads scope correctly ──────────────────────────────

    #[test]
    fn jomini_chain_state_owner_getname() {
        // [owner.GetName] from State scope: owner → Country → terminal
        let entry = make_entry_with_jomini(vec![vec![
            JominiCommand {
                key: "owner".into(),
                params: Vec::new(),
            },
            JominiCommand {
                key: "GetName".into(),
                params: Vec::new(),
            },
        ]]);
        let data = hoi4_data();
        let diags = validate_loc_commands(&entry, ScopeId(101), &data);
        assert!(
            diags.is_empty(),
            "owner.GetName from State should be valid, got: {:?}",
            diags
        );
    }

    #[test]
    fn jomini_chain_wrong_scope_in_middle() {
        // [controller.GetName] from Country (100): controller is State-only
        let entry = make_entry_with_jomini(vec![vec![
            JominiCommand {
                key: "controller".into(),
                params: Vec::new(),
            },
            JominiCommand {
                key: "GetName".into(),
                params: Vec::new(),
            },
        ]]);
        let data = hoi4_data();
        let diags = validate_loc_commands(&entry, ScopeId(100), &data);
        assert!(
            !diags.is_empty(),
            "controller from Country should produce WrongScope, got: {:?}",
            diags
        );
    }

    // ── CW226: unknown final command when registry is present ─────────────────

    #[test]
    fn unknown_final_command_with_registry_emits_not_found() {
        // `totally_unknown` is not a scope link, not a getter (no "Get" prefix),
        // not in the terminal-commands list, and we have a registry.
        // Mirrors F# `LocNotFound` → CW226.
        let entry = make_entry_with_jomini(vec![vec![JominiCommand {
            key: "totally_unknown".into(),
            params: Vec::new(),
        }]]);
        let data = hoi4_data();
        let diags = validate_loc_commands(&entry, ScopeId(100), &data);
        assert_eq!(
            diags.len(),
            1,
            "expected one NotFound diagnostic: {:?}",
            diags
        );
        assert!(
            matches!(diags[0], LocCommandDiagnostic::NotFound { .. }),
            "expected NotFound, got: {:?}",
            diags
        );
    }

    #[test]
    fn unknown_final_command_without_registry_is_lenient() {
        // No registry → fully lenient, no CW226.
        let entry = make_entry_with_jomini(vec![vec![JominiCommand {
            key: "totally_unknown".into(),
            params: Vec::new(),
        }]]);
        let data = LocScopeData {
            game: Game::HOI4,
            terminal_commands: Vec::new(),
            question_mark_variable: true,
            parameter_variables: true,
            registry: None, // no registry
        };
        let diags = validate_loc_commands(&entry, ScopeId(0), &data);
        assert!(
            diags.is_empty(),
            "without registry, unknown command should be accepted: {:?}",
            diags
        );
    }

    #[test]
    fn get_prefixed_command_not_flagged_as_not_found() {
        // `GetSomethingCustom` starts with "Get" — treated as terminal, not CW226.
        let entry = make_entry_with_jomini(vec![vec![JominiCommand {
            key: "GetSomethingCustom".into(),
            params: Vec::new(),
        }]]);
        let data = hoi4_data();
        let diags = validate_loc_commands(&entry, ScopeId(100), &data);
        assert!(
            diags.is_empty(),
            "Get-prefixed command should be accepted as terminal: {:?}",
            diags
        );
    }
}
