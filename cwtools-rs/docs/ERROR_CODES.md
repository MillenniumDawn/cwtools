# CWTools Error Codes

Each diagnostic the validator emits carries a `CWxxx` code. The codes mirror the F# cwtools catalog with a few intentional renumberings (see [Reconciliations](#reconciliations) below). They are emitted from `crates/validation` and surfaced through the LSP server as editor diagnostics or printed to stdout by the CLI.

## Severity levels

| Severity | LSP equivalent | Typical use |
|---|---|---|
| Error | `error` | Definite defect -- game will likely not load or behave correctly |
| Warning | `warning` | Probable problem, or a thing that's usually wrong |
| Information | `information` | Style / performance hint; not a defect |
| Hint | `hint` | Low-priority suggestion |

## How to read an entry

**Status** values used below:

- **Emitted** -- the check runs and produces this code today.
- **Emitted (reconciled from F# CWxxx)** -- emitted today under the F# ID after an intentional renumber; the old Rust-invented ID is retired.
- **Defined, emission pending (subsystem)** -- the const exists but the check is not wired in; see [Pending subsystems](#currently-not-emitted-pending-subsystems).
- **Defined, not wired** -- the const exists but nothing emits it yet. Either it's superseded by a more specific code, or a generic check would need a complete registry to stay false-positive-safe (the project never trades correctness for coverage).
- **Emitted (escape hatch `CWTOOLS_...`)** -- runs by default; the env var disables it.

Every F# code (71 after the experimental/dead cleanup) has a Rust definition.
Codes with no emission site in either engine were removed from both; see
[Removed](#removed-experimental--dead-deleted-from-both-engines).

---

## CW001 -- Localisation parse error

| ID | Severity | Message | Meaning | Status |
|---|---|---|---|---|
| CW001 | Error | Localisation file parse error: {} | A line in a `.yml` loc file could not be parsed (no `:` separator). The parser recovers and continues; one diagnostic per bad line. Mirrors F# `validateLocalisationSyntax` / `YAMLLocalisationParser` `Failure` path. | Emitted |

---

## CW002 -- Core parser

| ID | Severity | Message | Meaning | Status |
|---|---|---|---|---|
| CW002 | Error | This block has mixed key/values and values, it is probably a missing equals sign inside it. | A block mixes bare values with key-value pairs, which usually means a missing `=`. | Defined, not wired (parser does not flag this yet) |

---

## CW100-CW122 -- Core: loc, variables, triggers/effects, scope, misc

| ID | Severity | Message | Meaning | Status |
|---|---|---|---|---|
| CW100 | Warning | Localisation key {} is not defined for {} | A referenced localisation key has no entry for the named language. | Emitted |
| CW101 | Error | {} is not defined | A `@variable` is used but never defined. | Defined, not wired (needs FP-safe @-var def/use tracking) |
| CW102 | Error | unknown trigger {} used. | A trigger name is not in the known trigger list. | Defined, not wired (rules-engine structural codes CW262-265 cover this; a generic check needs a complete trigger registry to avoid false positives) |
| CW103 | Error | unknown effect {} used. | An effect name is not in the known effect list. | Defined, not wired (as CW102, needs a complete effect registry) |
| CW104 | Error | {} trigger used in incorrect scope. In {} but expected {} | A trigger is used in a scope it doesn't accept. | Wired, gated off (`CWTOOLS_SCOPE_CHECKS=1`). Scope tracking handles links/iterators/data-refs/root-scope; a DLC-scope long tail remains, so off by default |
| CW105 | Error | {} effect used in incorrect scope. In {} but expected {} | An effect is used in the wrong scope. | Wired, gated off (`CWTOOLS_SCOPE_CHECKS=1`) |
| CW106 | Error | {} scope command used in incorrect scope. In {} but expected {} | A scope command is used outside its valid scope. | Wired, gated off (`CWTOOLS_SCOPE_CHECKS=1`) |
| CW107 | Information | Event is missing mean_time_to_happen, is_triggered_only, fire_only_once, or trigger={always=no}. Performance concern: event may fire every tick. | An event has no guard against running every tick. | Emitted (reconciled from F# CW107 / formerly Rust CW300) |
| CW108 | Error | This research_leader is missing required "area" | A `research_leader` block omits the required `area` field. | Emitted (Stellaris only; CW109 still defined, emission pending cross-block reasoning) |
| CW109 | Information | This research_leader uses area {} but the technology uses area {} | The area in `research_leader` disagrees with the linked technology's area. | Defined, emission pending (vanilla data registries) |
| CW110 | Error | No category found for this technology | A technology definition has no category. | Emitted (Stellaris only; matches `tech_<name> = { ... }` and `technology = { ... }` root blocks) |
| CW113 | Error | File {} not found, this is case sensitive | A file path referenced in script doesn't exist (case-sensitive check). | Emitted (FilepathField refs checked against the mod+vanilla file index) |
| CW120 | Information | Trigger {} can be made a pretrigger (see code action to fix) | A trigger that could be promoted to a pretrigger for performance. | Emitted (Stellaris only; global walker, fires alongside the event-scoped CW301) |
| CW121 | Warning | This 'if' trigger contains no effects | An `if` block contains only a `limit` or nothing at all. | Emitted |
| CW122 | Information | Localisation key {} should not be quoted when used inline, this can cause unexpected behaviour | A loc key is wrapped in quotes where it is used inline. | Emitted |

---

## CW220-CW276 -- Loc references, event targets, bool/syntax hints, rules engine, type system

### CW220-CW222 -- Event targets / event index

| ID | Severity | Message | Meaning | Status |
|---|---|---|---|---|
| CW220 | Error | {} or an event it calls require the event target(s) {} but they are not set by this event or by all possible events leading here | A required event target is never set on any path leading to this event. | Defined, emission pending (event-target dataflow + cross-file event index) |
| CW221 | Warning | {} or an event it calls require the event target(s) {} but they may not always be set by this event or by all possible events leading here | A required event target is not set on all paths leading to this event. | Defined, emission pending (event-target dataflow + cross-file event index) |
| CW222 | Warning | The event id {} is not defined | A reference to an event id (`<event>`) that has no definition. | Emitted (relabeled from CW500 for `<event>` type refs) |

### CW223 -- Boolean/syntax structural hints

| ID | Severity | Message | Meaning | Status |
|---|---|---|---|---|
| CW223 | Information | Do not use NOT with multiple children, replace this with either NOR or NAND to avoid ambiguity | `NOT` wraps more than one child, which is ambiguous. | Emitted |

### CW225-CW226 -- Localisation cross-references

| ID | Severity | Message | Meaning | Status |
|---|---|---|---|---|
| CW225 | Error | Localisation key "{}" references "{}" which doesn't exist in {} | A loc string's `$KEY$` reference points to a key that has no definition. | Emitted |
| CW226 | Error | Localisation key "{}" uses command "{}" which doesn't exist | A loc string's `[Command()]` single-segment Jomini call names a command not found in the scope registry (with a loaded registry). Multi-segment chains like `[THIS.var]` are lenient (scripted variables not indexed). Mirrors F# `validateJominiLocalisationCommandsBase` `LocNotFound`. | Emitted |

### CW227-CW233 -- Section/component/mesh/entity (Stellaris-specific)

| ID | Severity | Message | Meaning | Status |
|---|---|---|---|---|
| CW227 | Error | Section template {} can not be found | A ship design references a section template that doesn't exist. | Emitted (Stellaris only; walks `ship_design`/`global_ship_design` blocks, looks up via TypeIndex; CW228/CW230/CW233 still defined, emission pending per-template field data) |
| CW228 | Error | Section template {} does not have a slot {} | A section template is referenced with a slot name it doesn't define. | Defined, emission pending (vanilla data registries) |
| CW229 | Error | Component template {} can not be found | A ship design references a component template that doesn't exist. | Emitted (Stellaris only; walks `ship_design`/`global_ship_design` blocks, looks up via TypeIndex) |
| CW230 | Warning | Component and slot do not match, slot {} has size {} and component {} has size {} | The size of a component doesn't fit the slot it's placed in. | Defined, emission pending (vanilla data registries) |
| CW231 | Warning | Technology {} is not used | A technology definition is never referenced anywhere. | Defined, emission pending (cross-file reference tracking) |
| CW233 | Error | Entity {} is not defined | A section or other asset references an entity that isn't defined. | Defined, emission pending (vanilla data registries / asset index) |

### CW234-CW238 -- Loc placeholders, zero-modifier, Stellaris if/else

| ID | Severity | Message | Meaning | Status |
|---|---|---|---|---|
| CW234 | Information | Localisation key {} is a placeholder for {} | A loc value is `REPLACE_ME` or similar placeholder text. | Emitted |
| CW235 | Warning | Modifier {} has value 0. Modifiers are additive so likely doesn't do anything | A known modifier is set to `0`, which is a no-op for additive modifiers. | Emitted (fires on confirmed modifier names; rule-matched modifier fields not yet covered) |
| CW236 | Warning | Nested if/else in effects was deprecated with 2.1 and will be removed in a future release | Stellaris: nested `if/else` in effects, deprecated since 2.1. | Emitted |
| CW237 | Information | 2.1 changed nested if = { if else } behaviour in effects. Check this still works as expected | Stellaris: ambiguous nested `if = { if else }` after 2.1 behaviour change. | Emitted |
| CW238 | Error | An else/else_if is missing a preceding if | An `else` or `else_if` block appears with no matching `if` before it. | Emitted |

### CW239 -- Unused type

| ID | Severity | Message | Meaning | Status |
|---|---|---|---|---|
| CW239 | Warning | {} of type {} is not used anywhere, but is expected to be | A `should_be_referenced` type instance is never referenced in any other file. | Defined, emission pending (cross-file reference-tracking subsystem) (reconciled from Rust CW502) |

### CW240-CW249 -- Rules-engine dynamic codes

These are the core rules-engine codes. Severity and message text are computed per-rule (the rule's `## severity` option overrides the defaults here).

| ID | Severity | Message | Meaning | Status |
|---|---|---|---|---|
| CW240 | Error | {} | A value didn't match its rule's field type (int/float/enum/bool/date, etc.). | Emitted |
| CW241 | Error | {} | An unexpected property was found (generic). | Defined, not wired (superseded by the node-kind-specific CW262-CW265) |
| CW242 | Warning | {} | A field appears too few or too many times (cardinality violation). | Emitted |
| CW243 | Error | Target "{}" has incorrect scope. Is {} but expect {} | A scope target resolves to a scope the field doesn't expect. | Emitted (escape hatch `CWTOOLS_NO_SCOPE_CHECKS=1`) |
| CW244 | Error | {} is not a target. Expected a target in scope(s) {} | A value is not a valid target in any of the expected scopes. | Emitted (escape hatch `CWTOOLS_NO_SCOPE_CHECKS=1`) |
| CW245 | Error | Error in target. Link {} was used in scope {} but expected {} | A scope link inside a target chain was used in the wrong scope. | Emitted (escape hatch `CWTOOLS_NO_SCOPE_CHECKS=1`) |
| CW246 | Warning | The variable {} has not been set | A referenced variable hasn't been assigned anywhere the engine can see. | Wired, gated off (`CWTOOLS_VAR_CHECKS=1`; needs a complete mod+vanilla variable index) |
| CW247 | Error | Trigger/Effect/Modifier {} used in wrong scope. In {} but expect {} | A trigger, effect, or modifier rule was used in the wrong scope. | Emitted |
| CW248 | Error | Invalid scope command {} | A scope command is not valid here. | Emitted (escape hatch `CWTOOLS_NO_SCOPE_CHECKS=1`) |
| CW249 | Warning | Expecting a variable or number | A field required a variable reference or numeric literal but got something else. | Defined, emission pending (rare F# `changeScope` NotFound edge; the common unknown-variable case is CW246) |

### CW250-CW253 -- Game-specific: planet_killer, boolean, flag, set_name

| ID | Severity | Message | Meaning | Status |
|---|---|---|---|---|
| CW250 | Error | {} | A `planet_killer` definition is missing required configuration. | Emitted (Stellaris only; fires when `planet_killer = { ... }` is missing `type` or any of `planet_damage`/`armor_penetration`/`armor_damage`) |
| CW251 | Warning | This {} is unnecessary | A boolean operator (`AND`/`OR`) is nested directly inside an identical operator. | Emitted |
| CW253 | Information | Consider using "set_name" instead for consistency | `set_empire_name` or `set_planet_name` should be replaced with `set_name`. | Emitted |
| CW280 | Information | {} = { always = ... } matches the default and can be removed | HOI4 cleanup hint: a field whose body is exactly `{ always = <bool> }` matching the field's default (e.g. `allowed_civil_war = { always = no }`) is a no-op and can be deleted. Rust-original (no F# equivalent); field/default table in `per_game::hoi4`. | Emitted |

### CW254-CW268 -- Localisation file headers and content

| ID | Severity | Message | Meaning | Status |
|---|---|---|---|---|
| CW254 | Error | Localisation files must be UTF-8 BOM, this file is not | A `.yml` loc file is not encoded as UTF-8 with BOM. | Emitted |
| CW255 | Error | Localisation file name should contain (and ideally end with) "l_language.yml" | A loc file name contains no recognisable `l_xxx` language tag. | Emitted |
| CW256 | Error | Localisation file should start with "l_language:" on the first line (or a comment) | A loc file's first content line is not a language header. | Emitted |
| CW257 | Error | Localisation file's name has language {} doesn't match the header language {} | The language in the file name and the `l_xxx:` header disagree. | Emitted |
| CW258 | Information | Localisation file name should end with "l_language.yml" | Language tag is present but not at the end of the file name. F# defines this but leaves emission commented out as "only convention"; cwtools-rs matches that -- const defined, never fired. | Retired / not emitted |
| CW259 | Error | This localisation string refers to itself | A loc key's value includes a `$KEY$` reference back to the same key. | Emitted |
| CW260 | Error | Loc command {} used in wrong scope. In {} but expected {} | A loc command is used in a data scope that doesn't support it. | Emitted |
| CW261 | Error | Key {} of type {} is defined multiple times | A `unique` type key appears more than once across the loaded files. | Emitted (reconciled from Rust CW501) |
| CW262 | Error | {} | An unexpected `key = { ... }` node where the rule doesn't allow one. Also fires on a bad key inside a [math expression](MATH_EXPRESSIONS.md). | Emitted |
| CW263 | Error | {} | An unexpected `key = value` leaf where the rule doesn't allow one. Also fires on a mis-typed operator inside a [math expression](MATH_EXPRESSIONS.md). | Emitted |
| CW264 | Warning | {} | An unexpected bare value where the rule doesn't allow one. | Emitted |
| CW265 | Warning | {} | An unexpected `{ ... }` value clause where the rule doesn't allow one. | Emitted |
| CW266 | Error | Localisation key {} uses command {} which does not exist in data type {}. | A loc command is not valid in the resolved data type for that scope. | Emitted (reconciled from Rust CW262) |
| CW267 | Error | Expected a {} value, got {} | An alias key/value didn't match the expected alias category. | Emitted |
| CW268 | Warning | Localisation key {} doesn't start and end with double quotes | A loc value is missing its enclosing double-quote delimiters. | Emitted |

### CW269-CW276 -- Optimisation, precision, custom errors, inline scripts, invalid chars, key validation

| ID | Severity | Message | Meaning | Status |
|---|---|---|---|---|
| CW269 | Hint | Optimise by merging this with {} by using {} | Two lists could be merged for a minor script optimisation. | Defined, emission pending (vanilla data registries) |
| CW270 | Warning | Value too small, only 3 decimal places are supported in this context | A numeric value has more decimal places than the engine supports here. | Emitted (32-bit `variable_field` with >3 decimal places) |
| CW271 | Warning | Expected an integer | A field that requires an integer received a float or non-numeric value. | Emitted (`int_variable_field` given a fractional value) |
| CW272 | Error | {} | A custom error attached to a rule via `## error = ...`. | Defined, not wired (rules loader does not parse the `## error` option yet) |
| CW273 | Warning | Modifier type {} is not defined but is used | A modifier's type reference points to a modifier-type that isn't defined. | Defined, emission pending (modifier-type registry) |
| CW274 | Error | This usage of inline_script results in an error, see related | An `inline_script` call resolves to content that itself fails validation. | Defined, not wired (inline-script expansion does not propagate child errors yet) |
| CW275 | Warning | Localisation key {} contains unexpected characters, and may not render correctly | A loc value contains characters outside the expected set for that game. | Emitted |
| CW276 | Warning | Localisation key {} contains invalid characters (spaces or special characters are not allowed) | A loc key contains a space or character not valid in a loc key (only alphanumeric, `_`, `.`, `-` are allowed). Rust-only (no F# equivalent). | Emitted |

---

## CW301 -- Pre-trigger placement (Rust-only)

| ID | Severity | Message | Meaning | Status |
|---|---|---|---|---|
| CW301 | Warning | Pre-trigger '{}' should be inside a 'trigger' block, not at event root | A pre-trigger keyword appears at event root instead of inside a `trigger` block. No F# equivalent. | Emitted |

---

## CW400 -- Scope diagnostics (Rust-only)

| ID | Severity | Message | Meaning | Status |
|---|---|---|---|---|

---

## CW500 -- Type diagnostics (Rust-only)

| ID | Severity | Message | Meaning | Status |
|---|---|---|---|---|
| CW500 | Error | Type '{}' not found | A type name referenced in rules or script has no definition. No F# equivalent. | Emitted |

CW501 (duplicate type) and CW502 (unused type) were Rust-invented IDs that have been retired in favour of their F# equivalents CW261 and CW239 respectively.

---

## CW998-CW999 -- Internal and custom rule errors

| ID | Severity | Message | Meaning | Status |
|---|---|---|---|---|
| CW998 | Error | {} | An internal rules-engine error (malformed rule definition, etc.). | Defined, not wired (rules loader does not surface internal errors as diagnostics yet) |
| CW999 | Error | {} | A custom user error from a `.cwt` rule file. | Defined, not wired (needs the custom-error rule option) |

---

## Reconciliations

These are intentional ID renumberings documented in `error_codes.rs`. All converge Rust-invented codes onto their F# equivalents so downstream baselines key off a single consistent number.

- **CW501 -> CW261** (`DuplicateTypeDef`): cwtools-rs originally emitted duplicate-type errors as CW501; converged to F#'s CW261. CW501 is retired.
- **CW502 -> CW239** (`UnusedType`): cwtools-rs reserved CW502 for unused-type errors; converged to F#'s CW239. CW502 is retired.
- **CW300 -> CW107** (`EventEveryTick`): cwtools-rs originally emitted this as CW300 at Warning severity; F# emits it as CW107 at Information (performance hint, not a defect). Converged to CW107.
- **CW262 -> CW266** (loc-command-not-in-data-type): cwtools-rs originally used CW262 for `LocCommandNotInDataType`; CW262 belongs to F#'s `ConfigRulesUnexpectedPropertyNode`. Renumbered to CW266.
- **CW400 -> CW247** (`ConfigRulesRuleWrongScope`): the `## scope` rule-requirement check originally emitted the Rust-invented CW400; converged to F#'s CW247. CW400 is retired.
- **CW201-CW205 -> CW262-CW265 / CW240 / CW242** (rules-engine structural codes): cwtools-rs invented CW200-CW205 for rules-engine mismatches. These were replaced with the exact F# IDs: CW262/263/264/265 for the four node-kind-specific "unexpected property" variants, CW240 for unexpected value, and CW242 for cardinality violations.

---

## Currently not emitted (pending subsystems)

Every one of F#'s 71 codes has a Rust definition (full ID parity). The codes
below are defined but not yet emitted: each needs a subsystem Rust doesn't have
yet, and wiring it without that machinery would false-positive on valid game
config (which the project forbids). They are kept so only the emission site
remains to be built. None are HOI4/Millennium Dawn blockers; most are
Stellaris/other-game checks that need that game's corpus to validate.

| Subsystem needed | Codes blocked |
|---|---|
| Vanilla data registries (research_leader area / technology category) | CW108, CW109, CW110 |
| Pretrigger registry + scope engine | CW120 |
| Event-target dataflow + cross-file event index | CW220, CW221 |
| Ship sections / components (Stellaris asset model) | CW227, CW228, CW229, CW230, CW233 |
| Cross-file reference tracking (unused type / tech) | CW231, CW239 |
| Planet-killer config (Stellaris) | CW250 |
| List-merge optimisation hint | CW269 |
| Modifier-type registry | CW273 |
| Variable index edge (CW246 is wired + gated; CW249 is F#'s rare `changeScope` NotFound case) | CW249 |

### Wired, runs by default (with an escape hatch)

The scope family is config-driven and ON by default; set `CWTOOLS_NO_SCOPE_CHECKS=1`
to disable: **CW104, CW105, CW106, CW243, CW244, CW245, CW247, CW248, CW260**.

The "variable has not been set" check (**CW246**) is wired but OFF by default
(`CWTOOLS_VAR_CHECKS=1` to enable) until the mod+vanilla variable index is proven
complete for a corpus; on Millennium Dawn it surfaces ~99 genuine unset-variable
references plus some runtime/concatenated-name false positives still being triaged.

### Rust-only extensions (no F# equivalent)

- **CW301** — pre-trigger keyword at event root instead of inside a `trigger` block.
- **CW500** — an `<type>` reference that resolves to no known instance (the event-specific case is F#'s CW222).

### Removed (experimental / dead, deleted from both engines)

CW111, CW112, CW114, CW115, CW116, CW117, CW118, CW119, CW224, CW232 had no
emission site in F# (several were flagged "Experimental, please report errors").
They were deleted from the F# source and the Rust catalog. The retired
renumbering placeholders CW252 and CW400 were also removed. If button/sprite,
static-modifier, modifier, mesh, or undefined-script-variable validation is wanted
later, it is a fresh feature with a fresh code, not parity work. (CW117's
"variable never defined" intent is covered by the live CW246.)
