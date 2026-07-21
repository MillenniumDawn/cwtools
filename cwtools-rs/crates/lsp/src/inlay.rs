//! `textDocument/inlayHint`: annotate id references with their localised title.
//!
//! Two hint kinds were scoped for this feature (see the L9 review):
//!
//! - **Loc titles** (`cwtools.inlayHints.locTitles`, default ON): after a leaf
//!   whose value is a known type-instance id that has a localised title, render
//!   the title. Purely a pair of O(1) map hits per leaf — `is_any_instance` on
//!   the type index and a `loc_text` lookup — so it stays cheap over a viewport
//!   range without any new index.
//! - **Resolved scopes** (`cwtools.inlayHints.scopes`, default OFF): after a
//!   scope-changing key, render the scope it resolves to. NOT implemented: the
//!   resolved scope needs the ambient scope context at the leaf, which only the
//!   per-position resolver (`resolve_at_cursor` → `make_prepared` + `rules_at_pos`)
//!   produces. Running that per leaf across a whole range is exactly the
//!   too-costly path the review flagged, so the setting + capability are
//!   scaffolded but the handler produces no scope hints. Wiring the flag now
//!   keeps a later cheap-per-leaf implementation a drop-in.
//!
//! The capability is declared statically (loc titles default on); the handler
//! gates each kind on its setting and returns nothing when both are off.

use std::collections::HashMap;

use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::{
    InlayHint, InlayHintLabel, InlayHintParams, PositionEncodingKind, Range,
};

use cwtools_info::TypeIndex;
use cwtools_localization::Lang;
use cwtools_parser::ast::{Arena, Child, ParsedFile, Value};
use cwtools_string_table::string_table::StringTable;

use crate::Backend;
use crate::navigation::{value_col_in_line, value_start_after_eq};
use crate::paths::source_position_to_lsp;

/// Upper bound on hints returned for one request, so a huge visible range (or a
/// file that is one giant list of ids) can't produce an unbounded response.
const MAX_HINTS: usize = 200;

/// Longest localised title rendered inline before it is truncated with an
/// ellipsis. Titles can be full sentences; a hint that long stops being a hint.
const MAX_TITLE_CHARS: usize = 60;

impl Backend {
    pub(crate) async fn inlay_hint_impl(
        &self,
        params: InlayHintParams,
    ) -> Result<Option<Vec<InlayHint>>> {
        use std::sync::atomic::Ordering::Relaxed;
        let loc_titles = self.state.inlay_hints_loc_titles.load(Relaxed);
        // Scope hints are scaffolded (the setting + capability exist) but not yet
        // produced — see the module docs. `scopes` is still read so a client that
        // enables only scope hints keeps the capability alive; it just yields
        // nothing until a cheap per-leaf resolver lands.
        let scopes = self.state.inlay_hints_scopes.load(Relaxed);
        if !loc_titles && !scopes {
            return Ok(None);
        }
        let uri = params.text_document.uri.to_string();
        // Loc / rule files aren't game ASTs — no id references to annotate.
        if crate::paths::is_loc_file(&uri) || crate::paths::is_cwt_file(&uri) {
            return Ok(None);
        }
        // Snapshot document AST + text first (locks `documents` briefly, then
        // releases) so neither is held while the index / loc guards are taken.
        let Some(ast) = self.ast_for(&uri) else {
            return Ok(None);
        };
        let Some(text) = self.file_text_for(&uri) else {
            return Ok(None);
        };
        let encoding = self.state.config.read().position_encoding.clone();
        let mut hints = Vec::new();
        if loc_titles {
            // Lock order: info_service -> loc_text (documents already released).
            let info = self.state.info_service.read();
            let loc_text = self.state.loc_text.read();
            hints = loc_title_hints(
                &ast,
                &self.state.string_table,
                &text,
                params.range,
                &info.type_index,
                &loc_text,
                &encoding,
            );
        }
        if hints.is_empty() {
            Ok(None)
        } else {
            Ok(Some(hints))
        }
    }
}

/// Compute the localised-title inlay hints for the leaves of `file` whose lines
/// fall inside `range`. Pure (no locks / IO) so the handler and its tests share
/// the exact mapping. A hint is produced for a leaf (or bare leaf-value) whose
/// string value is BOTH a known type-instance id (`is_any_instance`, O(1)) and a
/// key present in `loc_text` (O(1)); the title text is placed just after the
/// value token, using the same source→LSP conversion the other handlers use.
pub(crate) fn loc_title_hints(
    file: &ParsedFile,
    table: &StringTable,
    text: &str,
    range: Range,
    type_index: &TypeIndex,
    loc_text: &HashMap<String, Vec<(Lang, String)>>,
    encoding: &PositionEncodingKind,
) -> Vec<InlayHint> {
    let mut hints = Vec::new();
    // Split the source into lines ONCE per request. Positioning needs the leaf's
    // own line; doing `text.lines().nth(line0)` per candidate leaf would be
    // O(bytes-before-line) redone for every leaf. Index into this slice instead.
    let lines: Vec<&str> = text.lines().collect();
    let cx = Ctx {
        arena: &file.arena,
        table,
        text,
        lines: &lines,
        range,
        type_index,
        loc_text,
        encoding,
    };
    collect_hints(&file.root_children, &cx, &mut hints);
    hints
}

/// Immutable per-request context, so the recursive walk passes one reference
/// instead of nine positional args.
struct Ctx<'a> {
    arena: &'a Arena,
    table: &'a StringTable,
    text: &'a str,
    lines: &'a [&'a str],
    range: Range,
    type_index: &'a TypeIndex,
    loc_text: &'a HashMap<String, Vec<(Lang, String)>>,
    encoding: &'a PositionEncodingKind,
}

impl Ctx<'_> {
    fn line(&self, line0: u32) -> &str {
        self.lines.get(line0 as usize).copied().unwrap_or("")
    }

    fn in_range(&self, line0: u32) -> bool {
        line0 >= self.range.start.line && line0 <= self.range.end.line
    }
}

fn collect_hints(children: &[Child], cx: &Ctx<'_>, out: &mut Vec<InlayHint>) {
    for child in children {
        if out.len() >= MAX_HINTS {
            return;
        }
        match child {
            Child::Leaf(idx) => {
                let leaf = &cx.arena.leaves[*idx as usize];
                if let Value::Clause(inner) = &leaf.value {
                    // Recurse only when the clause's line span overlaps the range;
                    // `pos.end` may overshoot past `}` (harmless — it only widens
                    // the guard), so a non-overlapping subtree is still skipped.
                    let start0 = leaf.pos.start.line.saturating_sub(1);
                    let end0 = leaf.pos.end.line.saturating_sub(1);
                    if end0 >= cx.range.start.line && start0 <= cx.range.end.line {
                        collect_hints(inner, cx, out);
                    }
                    continue;
                }
                let line0 = leaf.pos.start.line.saturating_sub(1);
                if !cx.in_range(line0) {
                    continue;
                }
                // `key = value`: skip the key, start the token scan after the `=`.
                if let Some(hint) = hint_for_value(
                    &leaf.value,
                    cx,
                    line0,
                    Anchor::Keyed(leaf.pos.start.col as u32),
                ) {
                    out.push(hint);
                }
            }
            Child::LeafValue(idx) => {
                let lv = &cx.arena.leaf_values[*idx as usize];
                let line0 = lv.pos.start.line.saturating_sub(1);
                if !cx.in_range(line0) {
                    continue;
                }
                // A bare value token: annotate it (scan from its own start). An
                // anonymous nested block (`{ ... }` with no key) is also a
                // LeafValue but its value is a `Value::Clause`, which
                // `hint_for_value` rejects — we intentionally don't descend into
                // anonymous blocks (rare in game script; keyed clauses cover the
                // normal case via the Leaf branch above).
                if let Some(hint) =
                    hint_for_value(&lv.value, cx, line0, Anchor::Bare(lv.pos.start.col as u32))
                {
                    out.push(hint);
                }
            }
            Child::Comment(_) => {}
        }
    }
}

/// Where to begin scanning for the value token on the leaf's line. `Keyed` skips
/// past the `=` (a `key = value` leaf); `Bare` starts at the value's own column
/// (a keyless leaf-value). Both carry the relevant source column.
enum Anchor {
    Keyed(u32),
    Bare(u32),
}

/// Build the title hint for a scalar `value` on `line0`. `None` when the value
/// isn't a string, is not a known id with a localised title, or its token can't
/// be located on the line (e.g. a value that spans lines).
///
/// The value text is BORROWED from the string table (`with_string`, no
/// allocation) to run both gates; the only per-hint allocations — the lowercased
/// lookup key and the title `String` — happen after the `is_any_instance` gate
/// passes, so a non-id leaf costs a borrow + one hash lookup and nothing more.
fn hint_for_value(value: &Value, cx: &Ctx<'_>, line0: u32, anchor: Anchor) -> Option<InlayHint> {
    // Only real identifiers carry ids; numbers / bools / clauses never do.
    let id = match value {
        Value::String(t) | Value::QString(t) => t.normal,
        _ => return None,
    };
    cx.table
        .with_string(id, |s| {
            let name = s.trim_matches('"');
            // Both gates are O(1): a known instance of some type AND a key with a
            // localised title. `is_any_instance` lowercases internally.
            if name.is_empty() || !cx.type_index.is_any_instance(name) {
                return None;
            }
            // Past the gate: now allocate the lowercased lookup key + the title.
            let name_lc = name.to_ascii_lowercase();
            let title = truncate_title(cx.loc_text.get(&name_lc)?.first()?.1.as_str());

            let line = cx.line(line0);
            let from = match anchor {
                Anchor::Keyed(key_col) => value_start_after_eq(line, key_col).unwrap_or(key_col),
                Anchor::Bare(col) => col,
            };
            let col = value_col_in_line(line, name, from)?;
            let end_col = col + name.chars().count() as u32;
            let position = source_position_to_lsp(cx.text, line0, end_col, cx.encoding);

            Some(InlayHint {
                position,
                label: InlayHintLabel::String(title),
                kind: None,
                text_edits: None,
                tooltip: None,
                padding_left: Some(true),
                padding_right: None,
                data: None,
            })
        })
        .flatten()
}

/// Truncate a title to [`MAX_TITLE_CHARS`] scalar values, appending `…` when cut.
/// Counts chars (not bytes) so multi-byte titles aren't split mid-codepoint.
fn truncate_title(title: &str) -> String {
    if title.chars().count() <= MAX_TITLE_CHARS {
        return title.to_string();
    }
    let mut s: String = title.chars().take(MAX_TITLE_CHARS).collect();
    s.push('…');
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use cwtools_info::{SourceLocation, TypeInstance};
    use tower_lsp::lsp_types::Position;

    fn idx_with(type_name: &str, names: &[&str]) -> TypeIndex {
        let mut idx = TypeIndex::new();
        let mut per_type = HashMap::new();
        per_type.insert(
            type_name.to_string(),
            names
                .iter()
                .map(|n| TypeInstance {
                    name: n.to_string(),
                    location: SourceLocation {
                        line: 1,
                        col: 0,
                        end: (1, 0),
                    },
                    primary_loc_key: None,
                })
                .collect(),
        );
        idx.merge("file:///mod/x.txt", per_type);
        idx
    }

    fn loc(pairs: &[(&str, &str)]) -> HashMap<String, Vec<(Lang, String)>> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), vec![(Lang::English, v.to_string())]))
            .collect()
    }

    fn hints_for(
        text: &str,
        idx: &TypeIndex,
        loc: &HashMap<String, Vec<(Lang, String)>>,
    ) -> Vec<InlayHint> {
        let table = StringTable::new();
        let ast = cwtools_parser::parser::parse_string(text, &table).expect("parse");
        let range = Range::new(Position::new(0, 0), Position::new(1000, 0));
        loc_title_hints(
            &ast,
            &table,
            text,
            range,
            idx,
            loc,
            &PositionEncodingKind::UTF16,
        )
    }

    fn label(h: &InlayHint) -> &str {
        match &h.label {
            InlayHintLabel::String(s) => s,
            _ => panic!("expected a string label"),
        }
    }

    #[test]
    fn value_with_title_gets_a_hint_after_the_value() {
        // `add_ideas = my_idea`, `my_idea` is a known idea with a title.
        let text = "c = {\n    add_ideas = my_idea\n}\n";
        let idx = idx_with("idea", &["my_idea"]);
        let loc = loc(&[("my_idea", "My Idea")]);
        let hints = hints_for(text, &idx, &loc);
        assert_eq!(hints.len(), 1, "one titled id -> one hint");
        assert_eq!(label(&hints[0]), "My Idea");
        assert_eq!(hints[0].padding_left, Some(true));
        // Positioned just past `my_idea` on line 1 (0-based): "    add_ideas = my_idea"
        //                                                       col 16 ..= 23
        assert_eq!(hints[0].position, Position::new(1, 23));
    }

    #[test]
    fn bare_leaf_value_in_a_list_gets_a_hint() {
        // A bare id inside a clause (no key), e.g. a list of ideas.
        let text = "list = {\n    my_idea\n}\n";
        let idx = idx_with("idea", &["my_idea"]);
        let loc = loc(&[("my_idea", "My Idea")]);
        let hints = hints_for(text, &idx, &loc);
        assert_eq!(hints.len(), 1);
        assert_eq!(label(&hints[0]), "My Idea");
        assert_eq!(hints[0].position, Position::new(1, 11));
    }

    #[test]
    fn known_id_without_a_title_gets_no_hint() {
        // The id is indexed but has no loc entry — nothing to render.
        let text = "c = {\n    add_ideas = my_idea\n}\n";
        let idx = idx_with("idea", &["my_idea"]);
        let loc = loc(&[]);
        assert!(hints_for(text, &idx, &loc).is_empty());
    }

    #[test]
    fn loc_key_that_is_not_a_known_instance_gets_no_hint() {
        // A value with a matching loc key but no type definition (a flag, a raw
        // string) must not be annotated — the gate is "known id", not "any key".
        let text = "c = {\n    set_country_flag = my_idea\n}\n";
        let idx = TypeIndex::new();
        let loc = loc(&[("my_idea", "My Idea")]);
        assert!(hints_for(text, &idx, &loc).is_empty());
    }

    #[test]
    fn quoted_value_resolves_the_inner_token() {
        let text = "c = {\n    name = \"my_idea\"\n}\n";
        let idx = idx_with("idea", &["my_idea"]);
        let loc = loc(&[("my_idea", "My Idea")]);
        let hints = hints_for(text, &idx, &loc);
        assert_eq!(hints.len(), 1);
        assert_eq!(label(&hints[0]), "My Idea");
    }

    #[test]
    fn case_insensitive_id_and_key() {
        // Paradox ids are case-insensitive; the index + loc map are lowercased.
        let text = "c = {\n    add_ideas = MY_IDEA\n}\n";
        let idx = idx_with("idea", &["my_idea"]);
        let loc = loc(&[("my_idea", "My Idea")]);
        let hints = hints_for(text, &idx, &loc);
        assert_eq!(hints.len(), 1);
        assert_eq!(label(&hints[0]), "My Idea");
    }

    #[test]
    fn out_of_range_leaves_are_skipped() {
        let text = "a = my_idea\nb = my_idea\nc = my_idea\n";
        let idx = idx_with("idea", &["my_idea"]);
        let loc = loc(&[("my_idea", "My Idea")]);
        let table = StringTable::new();
        let ast = cwtools_parser::parser::parse_string(text, &table).expect("parse");
        // Restrict the range to the middle line only.
        let range = Range::new(Position::new(1, 0), Position::new(1, 100));
        let hints = loc_title_hints(
            &ast,
            &table,
            text,
            range,
            &idx,
            &loc,
            &PositionEncodingKind::UTF16,
        );
        assert_eq!(hints.len(), 1, "only the in-range leaf is hinted");
        assert_eq!(hints[0].position.line, 1);
    }

    #[test]
    fn hint_count_is_capped() {
        // More than MAX_HINTS titled ids in range -> exactly MAX_HINTS returned.
        let n = MAX_HINTS + 50;
        let mut text = String::from("list = {\n");
        for _ in 0..n {
            text.push_str("    my_idea\n");
        }
        text.push_str("}\n");
        let idx = idx_with("idea", &["my_idea"]);
        let loc = loc(&[("my_idea", "My Idea")]);
        let hints = hints_for(&text, &idx, &loc);
        assert_eq!(hints.len(), MAX_HINTS);
    }

    #[test]
    fn long_title_is_truncated() {
        let long = "x".repeat(MAX_TITLE_CHARS + 10);
        let text = "c = {\n    add_ideas = my_idea\n}\n";
        let idx = idx_with("idea", &["my_idea"]);
        let loc = loc(&[("my_idea", long.as_str())]);
        let hints = hints_for(text, &idx, &loc);
        assert_eq!(hints.len(), 1);
        let rendered = label(&hints[0]);
        assert!(rendered.ends_with('…'));
        assert_eq!(rendered.chars().count(), MAX_TITLE_CHARS + 1);
    }

    #[test]
    fn numeric_and_bool_values_are_ignored() {
        let text = "c = {\n    cost = 5\n    flag = yes\n}\n";
        // Even if "5"/"true" were somehow indexed, non-string values are skipped.
        let mut idx = idx_with("idea", &["5"]);
        {
            let mut pt = HashMap::new();
            pt.insert(
                "idea".to_string(),
                vec![TypeInstance {
                    name: "true".to_string(),
                    location: SourceLocation {
                        line: 1,
                        col: 0,
                        end: (1, 0),
                    },
                    primary_loc_key: None,
                }],
            );
            idx.merge("file:///mod/y.txt", pt);
        }
        let loc = loc(&[("5", "Five"), ("true", "True")]);
        assert!(hints_for(text, &idx, &loc).is_empty());
    }
}
