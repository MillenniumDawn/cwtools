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
        .map(|s| s.trim_matches('#').trim().to_string())
        .collect();
    match desc_lines.len() {
        0 => None,
        1 => Some(desc_lines[0].clone()),
        _ => Some(desc_lines.join("\n")),
    }
}

/// Parse Options from comment lines preceding a rule.
/// CRITICAL: when NO cardinality comment is present, use min=1, max=1, strict_min=true (F# default).
pub(crate) fn options_from_comments(comments: &[String], is_comparison: bool) -> Options {
    // Cardinality: match by Contains("cardinality") (handles both ## and ##cardinality= forms)
    let (min, max, strict_min) =
        if let Some(c) = comments.iter().find(|s| s.contains("cardinality")) {
            // Extract everything after the '='
            if let Some(eq_idx) = c.find('=') {
                let spec = c[eq_idx + 1..].trim();
                if let Some((min_s, max_s)) = spec.split_once("..") {
                    let min_s = min_s.trim();
                    // Handle ~ prefix for strict_min=false
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
                (1, 1, true)
            }
        } else {
            // No cardinality comment -> F# default: 1..1, strict
            (1, 1, true)
        };

    // Description: all ## lines joined
    let description = extract_description_from_comments(comments);

    // push_scope: match by Contains("push_scope")
    let push_scope = comments
        .iter()
        .find(|s| s.contains("push_scope"))
        .and_then(|s| s.find('=').map(|i| s[i + 1..].trim().to_string()))
        .filter(|s| !s.is_empty());

    // replace_scopes / replace_scope: hand-parse key=value pairs
    let replace_scopes = parse_replace_scopes_from_comments(comments);

    // severity
    let severity = comments
        .iter()
        .find(|s| s.contains("severity"))
        .and_then(|s| s.find('=').map(|i| s[i + 1..].trim().to_string()))
        .and_then(|sev| match sev.as_str() {
            "error" => Some(Severity::Error),
            "warning" => Some(Severity::Warning),
            "info" | "information" => Some(Severity::Information),
            "hint" => Some(Severity::Hint),
            _ => None,
        });

    // required_scopes: # scope = X or # scope = { A B }
    let required_scopes = parse_required_scopes(comments);

    // reference_details
    let reference_details = if let Some(c) = comments
        .iter()
        .find(|s| s.contains("outgoingReferenceLabel"))
    {
        c.find('=').map(|i| (true, c[i + 1..].trim().to_string()))
    } else if let Some(c) = comments
        .iter()
        .find(|s| s.contains("incomingReferenceLabel"))
    {
        c.find('=').map(|i| (false, c[i + 1..].trim().to_string()))
    } else {
        None
    };

    // error_if_only_match
    let error_if_only_match = comments
        .iter()
        .find(|s| s.contains("error_if_only_match"))
        .and_then(|s| s.find('=').map(|i| s[i + 1..].trim().to_string()))
        .filter(|s| !s.is_empty());

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
        key_required_quotes: false,
        value_required_quotes: false,
        type_hint: None,
        error_if_only_match,
    }
}

pub(crate) fn parse_replace_scopes_from_comments(comments: &[String]) -> Option<ReplaceScopes> {
    let line = comments.iter().find(|s| s.contains("replace_scope"))?;

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
    // The parser keeps the leading `#`s in comment text, so a `## scope = X`
    // annotation arrives as "## scope = X". Strip the `#`s + whitespace, then
    // match the bare `scope` directive (NOT `push_scope` / `replace_scope`,
    // which don't start with "scope").
    //
    // Scan from the END: comments accumulate across commented-out rules
    // (`# alias[...]`), so the `## scope` closest to this rule (the last one) is
    // the relevant one, not an earlier orphaned annotation.
    for c in comments.iter().rev() {
        let t = c.trim_start_matches('#').trim();
        let Some(rest) = t.strip_prefix("scope") else {
            continue;
        };
        let Some(rhs) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        let rhs = rhs.trim();
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
