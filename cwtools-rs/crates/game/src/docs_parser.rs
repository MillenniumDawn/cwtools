//! Parsers for game-generated documentation / log files.
//!
//! Three formats are covered:
//!
//! 1. **Legacy docs** (CK2/EU4/HOI4/VIC2) — `trigger_docs.log` /
//!    `effect_docs.log`.  Header `DOCUMENTATION ==`, then one entry per
//!    block of: `name - desc`, `Usage …`, `Supported scopes: …`,
//!    optionally `Supported targets: …`, footer `==================`.
//!    Parsed by `parse_legacy_docs`.
//!
//! 2. **Jomini docs** (CK3/IR/VIC3/EU5) — `triggers.log` / `effects.log`.
//!    Header `Trigger Documentation:` or `Effect Documentation:`, then
//!    blocks separated by `--------------------`.  Each block has:
//!    `name - desc`, optional `Traits: …`, optional `Supported Scopes: …`,
//!    optional `Supported Targets: …`.  Parsed by `parse_jomini_triggers`
//!    and `parse_jomini_effects`.
//!
//! 3. **Stellaris modifier log** — `modifiers.log`.  After
//!    `Printing Modifier Definitions:`, one line per modifier:
//!    `- tag, Category: Cat`.  Parsed by `parse_modifier_log`.
//!
//! 4. **Jomini data-type dump** — produced by some Jomini games.
//!    Parsed by `parse_data_type_dump` (basic).

// ── Result types ─────────────────────────────────────────────────────────────

/// A single entry from a trigger/effect doc file.
#[derive(Debug, Clone, PartialEq)]
pub struct RawDoc {
    /// Script identifier, e.g. `has_technology`.
    pub name: String,
    /// Free-text description (first line after the ` - ` separator).
    pub desc: String,
    /// Scope names from `Supported Scopes:` / `Supported scopes:`.
    /// Empty list means the entry is valid in all scopes.
    pub scopes: Vec<String>,
    /// Target scope names from `Supported Targets:` / `Supported targets:`.
    pub targets: Vec<String>,
    /// Trait annotation, e.g. `yes/no` or `<, <=, =, !=, >, >=`.
    pub traits: Option<String>,
}

/// Whether the entry is a trigger, effect, or value-trigger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocKind {
    Trigger,
    /// A trigger whose value can be compared with `< <= = != > >=`.
    ValueTrigger,
    Effect,
}

/// A parsed doc entry with its kind annotated.
#[derive(Debug, Clone, PartialEq)]
pub struct DocEntry {
    pub kind: DocKind,
    pub raw: RawDoc,
}

impl DocEntry {
    pub fn name(&self) -> &str {
        &self.raw.name
    }

    pub fn scopes(&self) -> &[String] {
        &self.raw.scopes
    }

    pub fn targets(&self) -> &[String] {
        &self.raw.targets
    }
}

/// A parsed modifier definition (from setup.log or modifiers.log).
#[derive(Debug, Clone, PartialEq)]
pub struct ActualModifier {
    /// The modifier tag, e.g. `pop_happiness`.
    pub tag: String,
    /// Category name(s), e.g. `["Pops"]` or `["Country"]`.
    pub categories: Vec<String>,
}

/// Parsed result of a Jomini data-type dump.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DataTypeDump {
    /// Global Promotes: identifier → type name.
    pub promotes: Vec<(String, String)>,
    /// Global Functions: identifier → return type.
    pub functions: Vec<(String, String)>,
    /// Types block: type name → list of (member, type).
    pub types: Vec<(String, Vec<(String, String)>)>,
}

// ── Legacy docs parser ────────────────────────────────────────────────────────

/// Parse a legacy `trigger_docs.log` / `effect_docs.log` pair (CK2/EU4/HOI4/
/// VIC2 format).
///
/// The file contains two sections separated by `=================`:
/// the trigger section first, then the effect section.  Both start with a
/// `DOCUMENTATION ==` header.
///
/// Returns `(triggers, effects)`.
pub fn parse_legacy_docs(text: &str) -> (Vec<DocEntry>, Vec<DocEntry>) {
    // Split on the footer line that separates triggers from effects.
    // The F# parser calls `docFile .>>. docFile` so it expects exactly two.
    let separator = "=================";
    let mut parts = text.splitn(3, separator);
    let trigger_text = parts.next().unwrap_or("");
    let effect_text = parts.next().unwrap_or("");

    let triggers = parse_legacy_section(trigger_text, DocKind::Trigger);
    let effects = parse_legacy_section(effect_text, DocKind::Effect);
    (triggers, effects)
}

fn parse_legacy_section(text: &str, kind: DocKind) -> Vec<DocEntry> {
    // Each entry ends at the next blank line after "Supported scopes:" block.
    // The F# grammar: name, usage (up to "Supported scopes:"), scopes (+targets).
    // We iterate over name lines: lines of the form `identchars - description`.
    let mut entries = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let n = lines.len();
    let mut i = 0;

    while i < n {
        let line = lines[i].trim();
        // Look for a name line: `word - rest`
        if let Some((name, desc)) = parse_name_line(line) {
            // Collect until we hit "Supported scopes:" or blank/separator
            let mut scopes: Vec<String> = Vec::new();
            let mut targets: Vec<String> = Vec::new();
            let mut j = i + 1;
            while j < n {
                let l = lines[j].trim();
                let ll = l.to_ascii_lowercase();
                if ll.starts_with("supported scopes:") {
                    let rest = l[l.find(':').unwrap_or(0) + 1..].trim();
                    scopes = parse_scope_list(rest);
                } else if ll.starts_with("supported targets:") {
                    let rest = l[l.find(':').unwrap_or(0) + 1..].trim();
                    targets = parse_scope_list(rest);
                } else if l.is_empty() || l.starts_with("===") {
                    break;
                }
                j += 1;
            }
            let raw = RawDoc {
                name: name.to_string(),
                desc: desc.to_string(),
                scopes,
                targets,
                traits: None,
            };
            entries.push(DocEntry { kind, raw });
            i = j;
        } else {
            i += 1;
        }
    }
    entries
}

fn parse_name_line(line: &str) -> Option<(&str, &str)> {
    // Format: `identchars - rest`
    // The ident is word chars + '_', the separator is ` - `.
    let sep = " - ";
    let pos = line.find(sep)?;
    let name = &line[..pos];
    // Validate name: must be non-empty and only word chars
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    let desc = &line[pos + sep.len()..];
    Some((name, desc))
}

fn parse_scope_list(s: &str) -> Vec<String> {
    if s.is_empty() || s.eq_ignore_ascii_case("none") || s.eq_ignore_ascii_case("all") {
        return vec![];
    }
    s.split_whitespace()
        .map(|t| t.trim_matches(',').to_ascii_lowercase())
        .filter(|t| !t.is_empty())
        .collect()
}

// ── Jomini docs parser ────────────────────────────────────────────────────────

/// Parse a Jomini `triggers.log` (begins with `Trigger Documentation:`).
pub fn parse_jomini_triggers(text: &str) -> Vec<DocEntry> {
    parse_jomini_section(text, "Trigger Documentation:", DocKind::Trigger)
}

/// Parse a Jomini `effects.log` (begins with `Effect Documentation:`).
pub fn parse_jomini_effects(text: &str) -> Vec<DocEntry> {
    parse_jomini_section(text, "Effect Documentation:", DocKind::Effect)
}

fn parse_jomini_section(text: &str, header: &str, default_kind: DocKind) -> Vec<DocEntry> {
    // Find header
    let start = match find_header(text, header) {
        Some(s) => s,
        None => return vec![],
    };
    let body = &text[start..];

    // Split on `--------------------` separators
    let separator = "--------------------";
    let blocks: Vec<&str> = body.split(separator).collect();

    let mut entries = Vec::new();
    for block in &blocks {
        if let Some(entry) = parse_jomini_block(block.trim(), default_kind) {
            entries.push(entry);
        }
    }
    entries
}

fn find_header(text: &str, header: &str) -> Option<usize> {
    let lower = text.to_ascii_lowercase();
    let hdr_lower = header.to_ascii_lowercase();
    let pos = lower.find(&hdr_lower)?;
    // Advance past the header line
    Some(
        text[pos..]
            .find('\n')
            .map(|nl| pos + nl + 1)
            .unwrap_or(pos + header.len()),
    )
}

fn parse_jomini_block(block: &str, default_kind: DocKind) -> Option<DocEntry> {
    if block.is_empty() {
        return None;
    }
    let lines: Vec<&str> = block.lines().collect();
    let n = lines.len();
    if n == 0 {
        return None;
    }

    // First non-empty line should be: `name - desc`
    let mut first_idx = 0;
    while first_idx < n && lines[first_idx].trim().is_empty() {
        first_idx += 1;
    }
    if first_idx >= n {
        return None;
    }

    let name_line = lines[first_idx].trim();
    // The jomini format allows the name to end right before ` - ` (no trailing desc)
    let (name, desc) = if let Some(sep_pos) = name_line.find(" - ") {
        let name = &name_line[..sep_pos];
        let desc = name_line[sep_pos + 3..].trim();
        (name, desc)
    } else {
        // Name only, no description
        let name = name_line;
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return None;
        }
        (name, "")
    };

    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }

    let mut traits: Option<String> = None;
    let mut scopes: Vec<String> = Vec::new();
    let mut targets: Vec<String> = Vec::new();

    for line in &lines[first_idx + 1..] {
        let l = line.trim();
        let ll = l.to_ascii_lowercase();
        if ll.starts_with("supported scopes:") {
            let rest = l[l.find(':').unwrap_or(0) + 1..].trim();
            scopes = parse_jomini_scope_list(rest);
        } else if ll.starts_with("supported targets:") {
            let rest = l[l.find(':').unwrap_or(0) + 1..].trim();
            targets = parse_jomini_scope_list(rest);
        } else if ll.starts_with("traits:") {
            let rest = l[l.find(':').unwrap_or(0) + 1..].trim();
            if !rest.is_empty() {
                traits = Some(rest.to_string());
            }
        }
    }

    // Determine kind: value trigger if traits contains comparison operators
    let kind = if default_kind == DocKind::Trigger {
        if traits
            .as_deref()
            .map(|t| {
                t.contains("<=")
                    || t.contains(">=")
                    || t.contains(", =,")
                    || t.contains("< ")
                    || t.contains("> ")
            })
            .unwrap_or(false)
        {
            DocKind::ValueTrigger
        } else {
            DocKind::Trigger
        }
    } else {
        default_kind
    };

    let raw = RawDoc {
        name: name.to_string(),
        desc: desc.to_string(),
        scopes,
        targets,
        traits,
    };
    Some(DocEntry { kind, raw })
}

fn parse_jomini_scope_list(s: &str) -> Vec<String> {
    if s.is_empty() || s.eq_ignore_ascii_case("none") || s.eq_ignore_ascii_case("all") {
        return vec![];
    }
    // Comma-separated: "province,character" or "province, character"
    s.split(',')
        .map(|t| t.trim().to_ascii_lowercase())
        .filter(|t| !t.is_empty())
        .collect()
}

// ── Stellaris modifier log parser ─────────────────────────────────────────────

/// Parse a Stellaris `modifiers.log`.
///
/// The relevant section starts with `Printing Modifier Definitions:` and
/// contains lines of the form:
/// ```text
/// - tag_name, Category: CategoryName
/// ```
///
/// Multiple categories per tag are comma-separated after the colon:
/// `Category: Pops, Planets`.
pub fn parse_modifier_log(text: &str) -> Vec<ActualModifier> {
    let marker = "Printing Modifier Definitions:";
    let start = match find_case_insensitive(text, marker) {
        Some(p) => p + marker.len(),
        None => return vec![],
    };
    let body = &text[start..];
    let mut result = Vec::new();

    for line in body.lines() {
        let l = line.trim();
        if !l.starts_with("- ") {
            // End of the modifier section (blank line, another header, etc.)
            if !l.is_empty() && !l.starts_with("--") && !l.starts_with('[') {
                // Allow non-modifier lines to be skipped silently
            }
            continue;
        }
        // Strip leading `- `
        let content = &l[2..];
        // Split on `, Category: ` or `, category: ` (case-insensitive)
        if let Some(cat_pos) = find_case_insensitive(content, ", Category:") {
            let tag = content[..cat_pos].trim().to_string();
            let cats_str = content[cat_pos + ", Category:".len()..].trim();
            let categories: Vec<String> = cats_str
                .split(',')
                .map(|c| c.trim().to_string())
                .filter(|c| !c.is_empty())
                .collect();
            if !tag.is_empty() {
                result.push(ActualModifier { tag, categories });
            }
        } else {
            // Line without a category annotation — treat as uncategorised
            let tag = content.trim_matches(',').trim().to_string();
            if !tag.is_empty() {
                result.push(ActualModifier {
                    tag,
                    categories: vec![],
                });
            }
        }
    }
    result
}

fn find_case_insensitive(haystack: &str, needle: &str) -> Option<usize> {
    let lower_h = haystack.to_ascii_lowercase();
    let lower_n = needle.to_ascii_lowercase();
    lower_h.find(&lower_n)
}

// ── Setup.log parser (Stellaris) ──────────────────────────────────────────────

/// Parse a Stellaris `setup.log`.
///
/// The file records static modifier entries like:
/// ```text
/// [timestamp] Static Modifier #N tag = TAG name = …
/// ```
/// …then a `Printing Modifier Definitions` section identical to modifiers.log.
///
/// We first try the `Printing Modifier Definitions` path; if absent we fall
/// back to extracting modifiers from `Static Modifier #N tag = TAG` lines.
pub fn parse_setup_log(text: &str) -> Vec<ActualModifier> {
    // Try the modifiers.log-style section first
    let from_modifier_section = parse_modifier_log(text);
    if !from_modifier_section.is_empty() {
        return from_modifier_section;
    }

    // Fallback: scan `Static Modifier #N tag = TAG` lines
    let mut result = Vec::new();
    for line in text.lines() {
        // Format: `[timestamp][source]: Static Modifier #N tag = TAG name = …`
        if let Some(tag_pos) = line.find("tag = ") {
            let rest = &line[tag_pos + 6..];
            let tag: String = rest.split_whitespace().next().unwrap_or("").to_string();
            if !tag.is_empty() {
                result.push(ActualModifier {
                    tag,
                    categories: vec![],
                });
            }
        }
    }
    result
}

// ── DataType dump parser ──────────────────────────────────────────────────────

/// Parse a Jomini data-type dump (basic: structure only, no semantic
/// interpretation).
///
/// Format:
/// ```text
/// Global Promotes = {
///   identifier -> TypeName
///   …
/// }
/// Global Functions = {
///   identifier -> TypeName
///   …
/// }
/// Types = {
///   TypeName = {
///     member -> TypeName
///     …
///   }
///   …
/// }
/// ```
pub fn parse_data_type_dump(text: &str) -> DataTypeDump {
    let mut dump = DataTypeDump::default();

    // Locate each top-level block
    if let Some(promotes) = extract_block(text, "Global Promotes") {
        dump.promotes = parse_arrow_pairs(promotes);
    }
    if let Some(functions) = extract_block(text, "Global Functions") {
        dump.functions = parse_arrow_pairs(functions);
    }
    if let Some(types_block) = extract_block(text, "Types") {
        dump.types = parse_types_block(types_block);
    }

    dump
}

/// Find `keyword = { … }` (outermost balanced braces) and return the content.
fn extract_block<'a>(text: &'a str, keyword: &str) -> Option<&'a str> {
    let search = format!("{} =", keyword);
    let lower = text.to_ascii_lowercase();
    let lower_search = search.to_ascii_lowercase();
    let start_kw = lower.find(&lower_search)?;
    let brace_start = text[start_kw..].find('{')? + start_kw;
    // Find the matching closing brace
    let content_start = brace_start + 1;
    let mut depth = 1usize;
    let mut pos = content_start;
    let bytes = text.as_bytes();
    while pos < bytes.len() && depth > 0 {
        match bytes[pos] {
            b'{' => depth += 1,
            b'}' => depth -= 1,
            _ => {}
        }
        pos += 1;
    }
    if depth == 0 {
        Some(&text[content_start..pos - 1])
    } else {
        None
    }
}

fn parse_arrow_pairs(text: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    for line in text.lines() {
        let l = line.trim();
        if let Some(arrow_pos) = l.find("->") {
            let key = l[..arrow_pos].trim().to_string();
            let val = l[arrow_pos + 2..].trim().to_string();
            if !key.is_empty() && !val.is_empty() {
                pairs.push((key, val));
            }
        }
    }
    pairs
}

fn parse_types_block(text: &str) -> Vec<(String, Vec<(String, String)>)> {
    let mut types = Vec::new();
    let bytes = text.as_bytes();
    let mut pos = 0;

    while pos < bytes.len() {
        // Skip whitespace
        while pos < bytes.len()
            && (bytes[pos] == b' '
                || bytes[pos] == b'\t'
                || bytes[pos] == b'\r'
                || bytes[pos] == b'\n')
        {
            pos += 1;
        }
        if pos >= bytes.len() {
            break;
        }
        // Read identifier up to `= {` or `={`
        let name_start = pos;
        while pos < bytes.len() && bytes[pos] != b'=' && bytes[pos] != b'\n' && bytes[pos] != b'\r'
        {
            pos += 1;
        }
        let name = std::str::from_utf8(&bytes[name_start..pos])
            .unwrap_or("")
            .trim()
            .to_string();
        if name.is_empty() {
            pos += 1;
            continue;
        }
        // Skip until `{`
        let brace_pos = match text[pos..].find('{') {
            Some(p) => pos + p,
            None => break,
        };
        let content_start = brace_pos + 1;
        let mut depth = 1usize;
        let mut end_pos = content_start;
        while end_pos < bytes.len() && depth > 0 {
            match bytes[end_pos] {
                b'{' => depth += 1,
                b'}' => depth -= 1,
                _ => {}
            }
            end_pos += 1;
        }
        let inner = &text[content_start..end_pos.saturating_sub(1)];
        let pairs = parse_arrow_pairs(inner);
        types.push((name, pairs));
        pos = end_pos;
    }
    types
}

// ── Modifier definitions transform ───────────────────────────────────────────

/// Turn a list of `ActualModifier` records into a list of
/// `(tag, categories)` tuples suitable for registering with the validation
/// layer.
///
/// This is a pure data transform — it does not touch any validation state.
/// The validation crate is expected to consume the output and call
/// `register_modifier_key` (or equivalent) for each entry.
pub fn modifier_definitions_from_docs(modifiers: &[ActualModifier]) -> Vec<ModifierDef> {
    modifiers
        .iter()
        .map(|m| ModifierDef {
            tag: m.tag.clone(),
            categories: m.categories.clone(),
        })
        .collect()
}

/// A modifier key with its associated scope categories, ready for validation
/// registration.
#[derive(Debug, Clone, PartialEq)]
pub struct ModifierDef {
    pub tag: String,
    pub categories: Vec<String>,
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Jomini tests against real IR log files ────────────────────────────────

    /// Test path helpers — resolve relative to the workspace root.
    fn ir_log_dir() -> std::path::PathBuf {
        // This file lives at cwtools-rs/crates/game/src/docs_parser.rs
        // The logs are at cwtools/CWToolsTests/testfiles/configtests/rulestests/IR/
        let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        // cwtools-rs/crates/game → go up 3 to cwtools root, then to tests
        manifest
            .parent() // crates
            .and_then(|p| p.parent()) // cwtools-rs
            .and_then(|p| p.parent()) // cwtools
            .map(|p| {
                p.join("CWToolsTests/testfiles/configtests/rulestests/IR")
            })
            .unwrap_or_else(|| std::path::PathBuf::from(
                "/mnt/Linux/github-projects/cwtools/CWToolsTests/testfiles/configtests/rulestests/IR"
            ))
    }

    #[test]
    fn jomini_effects_nonzero() {
        let dir = ir_log_dir();
        let path = dir.join("effects.log");
        if !path.exists() {
            // Silently skip if the test fixture is not present (CI without full repo)
            return;
        }
        let text = std::fs::read_to_string(&path).expect("read effects.log");
        let effects = parse_jomini_effects(&text);
        assert!(
            !effects.is_empty(),
            "expected at least one effect from {}",
            path.display()
        );
        // Every entry should have a non-empty name
        for e in &effects {
            assert!(!e.name().is_empty(), "entry with empty name");
        }
        // Spot-check known entry
        let set_var = effects.iter().find(|e| e.name() == "set_local_variable");
        assert!(set_var.is_some(), "expected set_local_variable effect");
    }

    #[test]
    fn jomini_triggers_nonzero() {
        let dir = ir_log_dir();
        let path = dir.join("triggers.log");
        if !path.exists() {
            return;
        }
        let text = std::fs::read_to_string(&path).expect("read triggers.log");
        let triggers = parse_jomini_triggers(&text);
        assert!(
            !triggers.is_empty(),
            "expected at least one trigger from {}",
            path.display()
        );
        // Spot-check: province_tax_income should be a value trigger (has <= / >= traits)
        let ptax = triggers.iter().find(|e| e.name() == "province_tax_income");
        assert!(ptax.is_some(), "expected province_tax_income trigger");
        if let Some(e) = ptax {
            assert_eq!(
                e.kind,
                DocKind::ValueTrigger,
                "province_tax_income should be ValueTrigger"
            );
        }
    }

    #[test]
    fn jomini_effects_have_scopes() {
        let dir = ir_log_dir();
        let path = dir.join("effects.log");
        if !path.exists() {
            return;
        }
        let text = std::fs::read_to_string(&path).expect("read effects.log");
        let effects = parse_jomini_effects(&text);
        // At least some effects must have non-empty scopes
        let with_scopes = effects.iter().filter(|e| !e.scopes().is_empty()).count();
        assert!(
            with_scopes > 0,
            "expected at least one effect with scope annotations"
        );
        // create_pop should have scope "province"
        let cp = effects.iter().find(|e| e.name() == "create_pop");
        if let Some(e) = cp {
            assert!(
                e.scopes().iter().any(|s| s == "province"),
                "create_pop should have scope 'province', got {:?}",
                e.scopes()
            );
        }
    }

    // ── Modifier log tests ────────────────────────────────────────────────────

    fn stellaris_modifiers_log() -> std::path::PathBuf {
        let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        manifest
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
            .map(|p| {
                p.join("CWToolsTests/testfiles/parsertests/stellarisnewdocs/modifiers.log")
            })
            .unwrap_or_else(|| std::path::PathBuf::from(
                "/mnt/Linux/github-projects/cwtools/CWToolsTests/testfiles/parsertests/stellarisnewdocs/modifiers.log"
            ))
    }

    #[test]
    fn modifier_log_nonzero() {
        let path = stellaris_modifiers_log();
        if !path.exists() {
            return;
        }
        let text = std::fs::read_to_string(&path).expect("read modifiers.log");
        let mods = parse_modifier_log(&text);
        assert!(
            !mods.is_empty(),
            "expected at least one modifier from {}",
            path.display()
        );
        // Spot-check
        let pop_happy = mods.iter().find(|m| m.tag == "pop_happiness");
        assert!(pop_happy.is_some(), "expected pop_happiness modifier");
        if let Some(m) = pop_happy {
            assert!(
                m.categories.iter().any(|c| c.eq_ignore_ascii_case("Pops")),
                "pop_happiness should have category Pops, got {:?}",
                m.categories
            );
        }
    }

    #[test]
    fn modifier_defs_transform() {
        let mods = vec![
            ActualModifier {
                tag: "pop_happiness".to_string(),
                categories: vec!["Pops".to_string()],
            },
            ActualModifier {
                tag: "ship_hull_add".to_string(),
                categories: vec!["Ships".to_string()],
            },
        ];
        let defs = modifier_definitions_from_docs(&mods);
        assert_eq!(defs.len(), 2);
        assert_eq!(defs[0].tag, "pop_happiness");
        assert_eq!(defs[1].categories, vec!["Ships".to_string()]);
    }

    // ── Legacy format unit tests (inline) ────────────────────────────────────

    #[test]
    fn legacy_basic_parse() {
        let text = r#"
DOCUMENTATION ==
has_technology - Does the country have this technology?
Usage: has_technology = TAG
Supported scopes: country
Supported targets:

=================
DOCUMENTATION ==
add_technology - Adds technology to the country.
Supported scopes: country

=================
"#;
        let (triggers, effects) = parse_legacy_docs(text);
        assert_eq!(triggers.len(), 1, "expected 1 trigger");
        assert_eq!(triggers[0].name(), "has_technology");
        assert_eq!(triggers[0].scopes(), &["country"]);
        assert_eq!(effects.len(), 1, "expected 1 effect");
        assert_eq!(effects[0].name(), "add_technology");
    }

    // ── Jomini inline tests ───────────────────────────────────────────────────

    #[test]
    fn jomini_inline_trigger() {
        let text = "Trigger Documentation:\n\n--------------------\n\nis_alive - Is the character alive?\nTraits: yes/no\nSupported Scopes: character\n\n--------------------\n\ncountry_population - The total population of a country\nTraits: <, <=, =, !=, >, >=\nSupported Scopes: country\n\n--------------------\n";
        let triggers = parse_jomini_triggers(text);
        assert_eq!(triggers.len(), 2);
        let alive = &triggers[0];
        assert_eq!(alive.name(), "is_alive");
        assert_eq!(alive.kind, DocKind::Trigger);
        assert_eq!(alive.scopes(), &["character"]);
        let pop = &triggers[1];
        assert_eq!(pop.name(), "country_population");
        assert_eq!(pop.kind, DocKind::ValueTrigger);
    }

    #[test]
    fn jomini_inline_effect() {
        let text = "Effect Documentation:\n\n--------------------\n\ncreate_pop - Creates a pop\nSupported Scopes: province\nSupported Targets: none\n\n--------------------\n";
        let effects = parse_jomini_effects(text);
        assert_eq!(effects.len(), 1);
        assert_eq!(effects[0].name(), "create_pop");
        assert_eq!(effects[0].scopes(), &["province"]);
    }

    // ── Data type dump inline test ────────────────────────────────────────────

    #[test]
    fn data_type_dump_basic() {
        let text = r#"Global Promotes = {
    Character -> Character
    Country -> Country
}
Global Functions = {
    add -> Value
}
Types = {
    Character = {
        age -> Value
        name -> CString
    }
}
"#;
        let dump = parse_data_type_dump(text);
        assert_eq!(dump.promotes.len(), 2);
        assert_eq!(
            dump.promotes[0],
            ("Character".to_string(), "Character".to_string())
        );
        assert_eq!(dump.functions.len(), 1);
        assert_eq!(dump.types.len(), 1);
        assert_eq!(dump.types[0].0, "Character");
        assert_eq!(dump.types[0].1.len(), 2);
    }

    #[test]
    fn jomini_empty_header() {
        // No header → empty result
        let triggers = parse_jomini_triggers("some unrelated text");
        assert!(triggers.is_empty());
    }

    #[test]
    fn modifier_log_inline() {
        let text = "Printing Modifier Definitions:\n- pop_happiness, Category: Pops\n- ship_hull_add, Category: Ships\n";
        let mods = parse_modifier_log(text);
        assert_eq!(mods.len(), 2);
        assert_eq!(mods[0].tag, "pop_happiness");
        assert_eq!(mods[0].categories, vec!["Pops"]);
        assert_eq!(mods[1].tag, "ship_hull_add");
        assert_eq!(mods[1].categories, vec!["Ships"]);
    }

    // ── Jomini effects.log with "Supported Scopes: none" ─────────────────────
    #[test]
    fn jomini_scope_none_empty_vec() {
        let text = "Effect Documentation:\n\n--------------------\n\nset_local_variable - Sets a variable\nSupported Scopes: none\n\n--------------------\n";
        let effects = parse_jomini_effects(text);
        assert_eq!(effects.len(), 1);
        assert_eq!(effects[0].name(), "set_local_variable");
        // "none" → empty scope vec
        assert!(
            effects[0].scopes().is_empty(),
            "expected empty scopes for 'none'"
        );
    }

    // Path-based existence test so CI can skip gracefully
    #[test]
    fn ir_log_files_exist_or_skip() {
        let dir = ir_log_dir();
        if !dir.exists() {
            eprintln!("Skipping IR log path tests: {:?} not found", dir);
            return;
        }
        assert!(dir.join("effects.log").exists(), "effects.log missing");
        assert!(dir.join("triggers.log").exists(), "triggers.log missing");
    }
}
