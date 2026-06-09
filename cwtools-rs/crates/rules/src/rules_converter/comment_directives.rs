//! Parsing of the `#`/`##`/`###` comment directives that precede rules: rule
//! `Options` (cardinality, scope, severity, ...), `replace_scope(s)`, required
//! scopes, and the documentation lines used for hover tooltips.

use super::*;

/// Extract description from ### comments (## are options).
pub(crate) fn extract_description_from_comments(comments: &[String]) -> Option<String> {
    // Only `###` lines are documentation. `##` lines are rule options
    // (cardinality, scope, severity, ...) and must NOT leak into the hover
    // tooltip. This intentionally diverges from F# (RulesParser.fs collects
    // every `##` line), which polluted every tooltip with option text.
    let desc_lines: Vec<String> = comments
        .iter()
        .filter(|s| s.starts_with("###"))
        .map(|s| s.trim_start_matches('#').trim().to_string())
        .collect();
    match desc_lines.len() {
        0 => None,
        1 => Some(desc_lines[0].clone()),
        _ => Some(desc_lines.join("\n")),
    }
}

/// True when a bare `## key` or `## key = ...` directive appears in exactly-`##` comment lines.
///
/// Used for boolean flags (`## required`, `## optional`, `## primary`) that may
/// appear without an `= value` suffix.
pub(crate) fn has_directive(comments: &[String], key: &str) -> bool {
    for c in comments.iter().rev() {
        let Some(rest) = c.strip_prefix("##") else {
            continue;
        };
        if rest.starts_with('#') {
            continue;
        }
        let rest = rest.trim_start();
        // Matches bare `## key` or `## key = ...` or `## key_something` — use
        // word-boundary logic: key must be followed by end-of-string, '=', or whitespace.
        if let Some(after) = rest.strip_prefix(key)
            && (after.is_empty()
                || after.starts_with('=')
                || after.starts_with(|c: char| c.is_whitespace()))
        {
            return true;
        }
    }
    false
}

/// Return the value of a `key = value` directive from exactly-`##` comment lines.
///
/// Rules:
/// - The line must start with exactly `##` (two hashes), not `#` or `###`.
/// - After stripping `##` and whitespace, the line must begin with `key`.
/// - Newest match wins (scan from end).
///
/// Returns the RHS string (trimmed) when found, `None` otherwise.
pub(crate) fn find_directive<'a>(comments: &'a [String], key: &str) -> Option<&'a str> {
    for c in comments.iter().rev() {
        let Some(rest) = c.strip_prefix("##") else {
            continue;
        };
        // Exclude `###` lines — those are documentation.
        if rest.starts_with('#') {
            continue;
        }
        let rest = rest.trim_start();
        let Some(after_key) = rest.strip_prefix(key) else {
            continue;
        };
        let after_key = after_key.trim_start();
        let Some(rhs) = after_key.strip_prefix('=') else {
            continue;
        };
        return Some(rhs.trim());
    }
    None
}

/// Parse Options from comment lines preceding a rule.
/// CRITICAL: when NO cardinality comment is present, use min=1, max=1, strict_min=true (F# default).
pub(crate) fn options_from_comments(comments: &[String], is_comparison: bool) -> Options {
    // Cardinality: from exactly-## lines only, newest-match-wins.
    let (min, max, strict_min) = if let Some(spec) = find_directive(comments, "cardinality") {
        if let Some((min_s, max_s)) = spec.split_once("..") {
            let min_s = min_s.trim();
            let (min_s, strict) = match min_s.strip_prefix('~') {
                Some(rest) => (rest, false),
                None => (min_s, true),
            };
            let min = min_s.parse::<i32>().unwrap_or(1);
            let max = if max_s.trim() == "inf" {
                i32::MAX
            } else {
                max_s.trim().parse::<i32>().unwrap_or(1)
            };
            (min, max, strict)
        } else {
            (1, 1, true)
        }
    } else {
        // No cardinality comment -> F# default: 1..1, strict
        (1, 1, true)
    };

    // Description: all ## lines joined
    let description = extract_description_from_comments(comments);

    // push_scope: from exactly-## lines only, newest-match-wins.
    let push_scope = find_directive(comments, "push_scope")
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    // replace_scopes / replace_scope: hand-parse key=value pairs
    let replace_scopes = parse_replace_scopes_from_comments(comments);

    // severity: from exactly-## lines only.
    let severity = find_directive(comments, "severity").and_then(|sev| match sev {
        "error" => Some(Severity::Error),
        "warning" => Some(Severity::Warning),
        "info" | "information" => Some(Severity::Information),
        "hint" => Some(Severity::Hint),
        _ => None,
    });

    // required_scopes: ## scope = X or ## scope = { A B }
    let required_scopes = parse_required_scopes(comments);

    // reference_details: from exactly-## lines.
    let reference_details = find_directive(comments, "outgoingReferenceLabel")
        .map(|v| (true, v.to_string()))
        .or_else(|| {
            find_directive(comments, "incomingReferenceLabel").map(|v| (false, v.to_string()))
        });

    // error_if_only_match: from exactly-## lines.
    let error_if_only_match = find_directive(comments, "error_if_only_match")
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    Options {
        min,
        max,
        strict_min,
        leafvalue: false,
        description,
        push_scope,
        replace_scopes,
        severity,
        required_scopes,
        comparison: is_comparison,
        reference_details,
        error_if_only_match,
    }
}

pub(crate) fn parse_replace_scopes_from_comments(comments: &[String]) -> Option<ReplaceScopes> {
    // Use find_directive so only exactly-## lines are considered.
    // "replace_scopes" is longer so try it first to avoid "replace_scope" matching a prefix.
    let rhs = find_directive(comments, "replace_scopes")
        .or_else(|| find_directive(comments, "replace_scope"))?;

    // Reconstruct as "replace_scope = <rhs>" so the existing parser below still works.
    let reconstructed = format!("replace_scope = {}", rhs);
    let line = &reconstructed;

    // Hand-parse key=value pairs from the comment text
    // Strip leading # chars to get the content
    let content = line.trim_start_matches('#').trim();

    // Find replace_scope(s) = { ... } or replace_scope(s) = bare_value
    let rs_start = content.find("replace_scope").unwrap_or(0);
    let after_rs = &content[rs_start..];

    let eq_idx = after_rs.find('=')?;
    let rhs = after_rs[eq_idx + 1..].trim();

    let pairs_str = if rhs.starts_with('{') {
        let close = rhs.find('}')?;
        &rhs[1..close]
    } else {
        rhs
    };

    let mut root = None;
    let mut this = None;
    let mut froms = Vec::new();
    let mut prevs = Vec::new();

    // Parse space-separated key = value pairs
    let tokens: Vec<&str> = pairs_str.split_whitespace().collect();
    let mut ti = 0;
    while ti + 2 < tokens.len() {
        if tokens[ti + 1] == "=" {
            match tokens[ti] {
                "this" => this = Some(tokens[ti + 2].to_string()),
                "root" => root = Some(tokens[ti + 2].to_string()),
                "from" => {
                    if froms.is_empty() {
                        froms.push(tokens[ti + 2].to_string());
                    } else {
                        froms[0] = tokens[ti + 2].to_string();
                    }
                }
                "fromfrom" => {
                    while froms.len() < 2 {
                        froms.push(String::new());
                    }
                    froms[1] = tokens[ti + 2].to_string();
                }
                "fromfromfrom" => {
                    while froms.len() < 3 {
                        froms.push(String::new());
                    }
                    froms[2] = tokens[ti + 2].to_string();
                }
                "fromfromfromfrom" => {
                    while froms.len() < 4 {
                        froms.push(String::new());
                    }
                    froms[3] = tokens[ti + 2].to_string();
                }
                "prev" => {
                    if prevs.is_empty() {
                        prevs.push(tokens[ti + 2].to_string());
                    } else {
                        prevs[0] = tokens[ti + 2].to_string();
                    }
                }
                "prevprev" => {
                    while prevs.len() < 2 {
                        prevs.push(String::new());
                    }
                    prevs[1] = tokens[ti + 2].to_string();
                }
                "prevprevprev" => {
                    while prevs.len() < 3 {
                        prevs.push(String::new());
                    }
                    prevs[2] = tokens[ti + 2].to_string();
                }
                "prevprevprevprev" => {
                    while prevs.len() < 4 {
                        prevs.push(String::new());
                    }
                    prevs[3] = tokens[ti + 2].to_string();
                }
                _ => {}
            }
            ti += 3;
        } else {
            ti += 1;
        }
    }

    if root.is_none() && this.is_none() && froms.is_empty() && prevs.is_empty() {
        return None;
    }

    Some(ReplaceScopes {
        root,
        this,
        froms,
        prevs,
    })
}

pub(crate) fn parse_required_scopes(comments: &[String]) -> Vec<String> {
    // Use find_directive so only exactly-## lines are considered and
    // newest-match-wins (scanning from the end).
    if let Some(rhs) = find_directive(comments, "scope") {
        if rhs.starts_with('{') && rhs.ends_with('}') {
            return rhs[1..rhs.len() - 1]
                .split_whitespace()
                .map(|s| s.to_string())
                .collect();
        } else if !rhs.is_empty() {
            return vec![rhs.to_string()];
        }
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    // ── find_directive ────────────────────────────────────────────────────────

    #[test]
    fn find_directive_basic() {
        assert_eq!(
            find_directive(&s(&["## cardinality = 0..1"]), "cardinality"),
            Some("0..1")
        );
    }

    #[test]
    fn find_directive_ignores_single_hash() {
        // # cardinality = 0..1 is a plain comment — must be ignored
        assert_eq!(
            find_directive(&s(&["# cardinality = 0..1"]), "cardinality"),
            None
        );
    }

    #[test]
    fn find_directive_ignores_triple_hash() {
        // ### cardinality = 0..1 is a doc line — must be ignored
        assert_eq!(
            find_directive(&s(&["### cardinality = 0..1"]), "cardinality"),
            None
        );
    }

    #[test]
    fn find_directive_newest_wins() {
        // Two ## cardinality lines: last one (by position) wins
        let comments = s(&["## cardinality = 1..1", "## cardinality = 0..inf"]);
        assert_eq!(find_directive(&comments, "cardinality"), Some("0..inf"));
    }

    #[test]
    fn find_directive_scope_not_from_doc() {
        // A ### doc line mentioning "scope" must not be picked up as ## scope
        let comments = s(&["### scope = the owning country", "## scope = country"]);
        assert_eq!(find_directive(&comments, "scope"), Some("country"));
    }

    #[test]
    fn find_directive_plain_comment_cardinality_ignored() {
        // A # plain comment mentioning cardinality must not be parsed
        let comments = s(&["# this has cardinality = 1..2 inside a sentence"]);
        assert_eq!(find_directive(&comments, "cardinality"), None);
    }

    // ── parse_required_scopes ─────────────────────────────────────────────────

    #[test]
    fn required_scopes_single() {
        assert_eq!(
            parse_required_scopes(&s(&["## scope = country"])),
            vec!["country"]
        );
    }

    #[test]
    fn required_scopes_block() {
        assert_eq!(
            parse_required_scopes(&s(&["## scope = { country state }"])),
            vec!["country", "state"]
        );
    }

    #[test]
    fn required_scopes_single_hash_ignored() {
        assert!(parse_required_scopes(&s(&["# scope = country"])).is_empty());
    }

    #[test]
    fn required_scopes_triple_hash_ignored() {
        assert!(parse_required_scopes(&s(&["### scope = the owning country"])).is_empty());
    }
}
