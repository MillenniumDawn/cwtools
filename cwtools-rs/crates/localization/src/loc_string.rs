//! Localization string command parser.
//!
//! Parses `$ref$` references and `[command]` blocks inside loc strings.
//! Supports:
//! * `$ref_key$`                   – reference to another loc key
//! * `[command]`                   – simple command
//! * `[command|format]`            – command with format specifier
//! * `[Scope.Owner.GetName]`       – Jomini command chains (CK3/VIC3)
//! * `[function(param1, param2)]`  – Jomini function calls
//! * `[?variable]`                 – event_target / saved variable reference
//! * `[event_target:foo]`          – named event target reference
//!
//! Handles both the original Paradox syntax (`[GetName]`) and the newer
//! Jomini syntax.

/// Parsed element inside a loc string.
#[derive(Debug, Clone, PartialEq)]
pub enum LocElement {
    /// Plain text characters.
    Chars(String),
    /// `$ref$` reference to another loc key.
    Ref(String),
    /// `[command]` block (non-Jomini).
    Command(String),
    /// `[Scope.Owner.GetName]` Jomini command chain or function call.
    JominiCommand(Vec<JominiCommand>),
}

/// A single Jomini command / function call.
#[derive(Debug, Clone, PartialEq)]
pub struct JominiCommand {
    pub key: String,
    pub params: Vec<JominiParam>,
}

/// Parameter to a Jomini function.
#[derive(Debug, Clone, PartialEq)]
pub enum JominiParam {
    /// A string literal, e.g. `'foo'`.
    Literal(String),
    /// A nested command chain, e.g. `Scope.Owner`.
    Commands(Vec<JominiCommand>),
}

/// Parse a loc string and return all elements.
///
/// This is a tolerant hand-written parser that handles:
/// * unescaped `$` inside text
/// * nested brackets
/// * Jomini function chains
///
/// # Arguments
/// * `s` – the raw description string (may include surrounding quotes)
pub fn parse_loc_elements(s: &str) -> Vec<LocElement> {
    let mut elements = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0; // byte offset; always lands on a char boundary

    while i < bytes.len() {
        match bytes[i] {
            b'$' => {
                if let Some((elem, new_i)) = parse_ref(s, i) {
                    elements.push(elem);
                    i = new_i;
                } else {
                    let end = next_special(s, i + 1);
                    elements.push(LocElement::Chars(s[i..end].to_string()));
                    i = end;
                }
            }
            b'[' => {
                if let Some((elem, new_i)) = parse_bracket(s, i) {
                    elements.push(elem);
                    i = new_i;
                } else {
                    let end = next_special(s, i + 1);
                    elements.push(LocElement::Chars(s[i..end].to_string()));
                    i = end;
                }
            }
            b']' => {
                // A closing bracket with no matching open is literal text.
                // It is special (so `next_special` stops on it); consume the
                // single ASCII byte explicitly, otherwise the `_` arm below
                // would make no progress and loop forever (OOM).
                elements.push(LocElement::Chars("]".to_string()));
                i += 1;
            }
            _ => {
                // `bytes[i]` is a non-special byte at a char boundary, so
                // `next_special(s, i)` returns a position strictly greater than
                // `i` (it can't match at `i`) — the loop always advances, and
                // `i` stays on a char boundary. Searching from `i + 1` would be
                // unsafe: `i + 1` may fall inside a multi-byte UTF-8 sequence.
                let end = next_special(s, i);
                elements.push(LocElement::Chars(s[i..end].to_string()));
                i = end;
            }
        }
    }

    elements
}

/// Return the byte offset of the next `$`, `[`, or `]` at or after `start`,
/// or `s.len()` if none.  Safe because `$`/`[`/`]` are ASCII and can never
/// appear as a continuation byte of a multi-byte UTF-8 sequence.
fn next_special(s: &str, start: usize) -> usize {
    s.as_bytes()[start..]
        .iter()
        .position(|&b| matches!(b, b'$' | b'[' | b']'))
        .map(|off| start + off)
        .unwrap_or(s.len())
}

/// Parse a `$ref$` starting at `s[start]` where `s.as_bytes()[start] == b'$'`.
///
/// Mirrors F# `dollarColour`: the ref name ends at `|` or `$`.
/// So `$MY_KEY|Y$` yields `Ref("MY_KEY")`.
fn parse_ref(s: &str, start: usize) -> Option<(LocElement, usize)> {
    let bytes = s.as_bytes();
    let content_start = start + 1; // skip opening '$'

    // Find end of key: '|' or '$'
    let key_end = bytes[content_start..]
        .iter()
        .position(|&b| b == b'$' || b == b'|')
        .map(|off| content_start + off)?;

    let key = &s[content_start..key_end];

    // A literal `$` (e.g. a currency sign) followed by non-identifier text is
    // not a ref. Loc keys, modifier names and idea names are all `[A-Za-z0-9_.]`,
    // so reject anything else: `$[?var|-3]`, `$§Y[?VAR|0]§!`, `$5 today$`.
    // The caller then treats the `$` as literal text.
    if !is_loc_ref_key(key) {
        return None;
    }

    if bytes[key_end] == b'|' {
        // Skip colour suffix up to and including the closing '$'
        let after_pipe = key_end + 1;
        let close = bytes[after_pipe..]
            .iter()
            .position(|&b| b == b'$')
            .map(|off| after_pipe + off)?;
        Some((LocElement::Ref(key.to_string()), close + 1))
    } else {
        // bytes[key_end] == b'$' — consume it
        Some((LocElement::Ref(key.to_string()), key_end + 1))
    }
}

/// Whether `key` is a plausible `$ref$` name: non-empty and made only of
/// loc-key identifier characters (`[A-Za-z0-9_.]`). Loc keys, modifier names
/// and idea names all fit this; literal-`$` constructs (currency, colour codes,
/// `[?...]` brackets) do not.
fn is_loc_ref_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.')
}

/// Parse a `[...]` block starting at `s[start]` where `s.as_bytes()[start] == b'['`.
fn parse_bracket(s: &str, start: usize) -> Option<(LocElement, usize)> {
    let bytes = s.as_bytes();
    let mut depth = 1usize;
    let mut i = start + 1;

    while i < bytes.len() && depth > 0 {
        match bytes[i] {
            b'[' => depth += 1,
            b']' => depth -= 1,
            _ => {}
        }
        i += 1;
    }

    if depth != 0 {
        return None; // unmatched bracket
    }

    // i points one past the closing ']'; content is s[start+1..i-1]
    let content = &s[start + 1..i - 1];

    if (content.contains('.') || content.contains('('))
        && let Ok(commands) = parse_jomini(content)
    {
        return Some((LocElement::JominiCommand(commands), i));
    }

    let command = content.find('|').map(|p| &content[..p]).unwrap_or(content);

    Some((LocElement::Command(command.to_string()), i))
}

/// Parse Jomini command chain / function call.
///
/// Examples:
/// * `Scope.Owner.GetName`
/// * `GetName('param')`
/// * `GetName(Scope.Owner.GetAge)`
fn parse_jomini(input: &str) -> Result<Vec<JominiCommand>, String> {
    let mut commands = Vec::new();
    let mut current = String::new();
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '.' => {
                if !current.is_empty() {
                    commands.push(JominiCommand {
                        key: std::mem::take(&mut current),
                        params: Vec::new(),
                    });
                }
            }
            '(' => {
                let key = std::mem::take(&mut current);
                let params = parse_jomini_params(&mut chars)?;
                commands.push(JominiCommand { key, params });
            }
            ' ' | ',' => {}
            _ => current.push(ch),
        }
    }

    if !current.is_empty() {
        commands.push(JominiCommand {
            key: current,
            params: Vec::new(),
        });
    }

    Ok(commands)
}

fn parse_jomini_params(
    chars: &mut std::iter::Peekable<std::str::Chars>,
) -> Result<Vec<JominiParam>, String> {
    let mut params = Vec::new();
    let mut current = String::new();

    for ch in chars.by_ref() {
        match ch {
            ')' => {
                if !current.trim().is_empty() {
                    params.push(parse_jomini_param(&current)?);
                }
                return Ok(params);
            }
            ',' => {
                if !current.trim().is_empty() {
                    params.push(parse_jomini_param(&current)?);
                }
                current.clear();
            }
            _ => current.push(ch),
        }
    }

    Err("unclosed parenthesis in Jomini function".to_string())
}

fn parse_jomini_param(s: &str) -> Result<JominiParam, String> {
    let trimmed = s.trim();
    if trimmed.starts_with('\'') && trimmed.ends_with('\'') {
        Ok(JominiParam::Literal(
            trimmed[1..trimmed.len() - 1].to_string(),
        ))
    } else if trimmed.contains('.') {
        let commands = parse_jomini(trimmed)?;
        Ok(JominiParam::Commands(commands))
    } else {
        Ok(JominiParam::Literal(trimmed.to_string()))
    }
}

/* ======================================================================== */
/* Tests                                                                   */
/* ======================================================================== */

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_ref() {
        let elems = parse_loc_elements("$FOO$");
        assert_eq!(elems, vec![LocElement::Ref("FOO".to_string())]);
    }

    #[test]
    fn test_simple_command() {
        let elems = parse_loc_elements("[GetName]");
        assert_eq!(elems, vec![LocElement::Command("GetName".to_string())]);
    }

    #[test]
    fn test_command_with_format() {
        let elems = parse_loc_elements("[GetName|Y]");
        assert_eq!(elems, vec![LocElement::Command("GetName".to_string())]);
    }

    #[test]
    fn test_mixed_text_and_commands() {
        let elems = parse_loc_elements("Hello [GetName], welcome!");
        assert_eq!(elems.len(), 3);
        assert_eq!(elems[0], LocElement::Chars("Hello ".to_string()));
        assert_eq!(elems[1], LocElement::Command("GetName".to_string()));
        assert_eq!(elems[2], LocElement::Chars(", welcome!".to_string()));
    }

    #[test]
    fn test_ref_and_command() {
        let elems = parse_loc_elements("$TITLE$ [GetName]");
        assert_eq!(elems.len(), 3);
        assert_eq!(elems[0], LocElement::Ref("TITLE".to_string()));
        assert_eq!(elems[1], LocElement::Chars(" ".to_string()));
        assert_eq!(elems[2], LocElement::Command("GetName".to_string()));
    }

    #[test]
    fn test_jomini_chain() {
        let elems = parse_loc_elements("[Scope.Owner.GetName]");
        assert_eq!(elems.len(), 1);
        if let LocElement::JominiCommand(cmds) = &elems[0] {
            assert_eq!(cmds.len(), 3);
            assert_eq!(cmds[0].key, "Scope");
            assert_eq!(cmds[1].key, "Owner");
            assert_eq!(cmds[2].key, "GetName");
        } else {
            panic!("expected JominiCommand");
        }
    }

    #[test]
    fn test_jomini_function() {
        let elems = parse_loc_elements("[GetName('foo')]");
        assert_eq!(elems.len(), 1);
        if let LocElement::JominiCommand(cmds) = &elems[0] {
            assert_eq!(cmds.len(), 1);
            assert_eq!(cmds[0].key, "GetName");
            assert_eq!(cmds[0].params.len(), 1);
            assert_eq!(cmds[0].params[0], JominiParam::Literal("foo".to_string()));
        } else {
            panic!("expected JominiCommand");
        }
    }

    #[test]
    fn test_event_target() {
        let elems = parse_loc_elements("[event_target:foo]");
        assert_eq!(elems.len(), 1);
        assert_eq!(
            elems[0],
            LocElement::Command("event_target:foo".to_string())
        );
    }

    #[test]
    fn test_question_variable() {
        let elems = parse_loc_elements("[?var_name]");
        assert_eq!(elems.len(), 1);
        assert_eq!(elems[0], LocElement::Command("?var_name".to_string()));
    }

    #[test]
    fn test_ref_colour_suffix_stripped() {
        // $MY_KEY|Y$ should yield Ref("MY_KEY"), not Ref("MY_KEY|Y")
        let elems = parse_loc_elements("$MY_KEY|Y$");
        assert_eq!(elems, vec![LocElement::Ref("MY_KEY".to_string())]);
    }

    #[test]
    fn test_ref_no_colour_suffix() {
        // Plain ref without colour suffix still works
        let elems = parse_loc_elements("$MY_KEY$");
        assert_eq!(elems, vec![LocElement::Ref("MY_KEY".to_string())]);
    }

    #[test]
    fn test_stray_closing_bracket_terminates() {
        // Regression: `[cmd]]` has an extra `]`. A lone `]` is special, so the
        // old `_` arm called next_special(s, i) == i and looped forever pushing
        // empty Chars (OOM). It must now terminate and treat `]` as literal text.
        let elems = parse_loc_elements("[USA.GetName]], rest");
        // Last elements include the stray `]` and the trailing text.
        let joined: String = elems
            .iter()
            .map(|e| match e {
                LocElement::Chars(c) => c.clone(),
                _ => String::new(),
            })
            .collect();
        assert!(
            joined.contains(']'),
            "stray bracket kept as text: {elems:?}"
        );
        assert!(joined.contains(", rest"), "trailing text parsed: {elems:?}");
    }

    #[test]
    fn test_only_closing_bracket() {
        // A bare `]` must not loop.
        let elems = parse_loc_elements("]");
        assert_eq!(elems, vec![LocElement::Chars("]".to_string())]);
    }

    #[test]
    fn test_multibyte_text_with_stray_bracket() {
        // Cyrillic text (multi-byte) around a stray `]` — must not panic on a
        // non-char-boundary index and must terminate.
        let elems = parse_loc_elements("мнения[USA.GetName]], потому");
        assert!(!elems.is_empty());
    }

    #[test]
    fn test_ref_colour_in_mixed_string() {
        // Colour-suffixed ref inside mixed text
        let elems = parse_loc_elements("Hello $NAME|G$ world");
        assert_eq!(elems[0], LocElement::Chars("Hello ".to_string()));
        assert_eq!(elems[1], LocElement::Ref("NAME".to_string()));
        assert_eq!(elems[2], LocElement::Chars(" world".to_string()));
    }

    fn refs(s: &str) -> Vec<String> {
        parse_loc_elements(s)
            .into_iter()
            .filter_map(|e| match e {
                LocElement::Ref(r) => Some(r),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn test_currency_dollar_before_bracket_is_literal() {
        // `$[?var|-3]` — the `$` is a literal currency sign, the `[?..]` a command.
        // Two adjacent constructs (as in MD loc) used to let the second `$` close
        // the first, yielding a bogus Ref("[?...mandatory_funding").
        let s = "$[?united_nations_esco_mandatory_funding|-3]\n$[?united_nations_esco_optional_funding|-3]";
        assert!(refs(s).is_empty(), "no bogus ref: {:?}", parse_loc_elements(s));
        // The bracket still parses as a command.
        assert!(parse_loc_elements(s).iter().any(|e| matches!(e, LocElement::Command(c) if c.starts_with("?united_nations_esco_mandatory"))));
    }

    #[test]
    fn test_colour_code_prefix_not_a_ref() {
        // `$§Y[?GDPVAR|0]§!` — colour-code + bracket after a literal `$`. Must not
        // yield a Ref (the all-caps body would otherwise dodge the lowercase heuristic).
        let s = "$§Y[?GDPVAR|0]§!$x$";
        assert!(
            !refs(s).iter().any(|r| r.contains('[') || r.contains('§')),
            "no bogus ref with bracket/colour chars: {:?}",
            refs(s)
        );
    }

    #[test]
    fn test_stray_currency_dollars_not_refs() {
        assert!(refs("$5 and $10").is_empty(), "{:?}", refs("$5 and $10"));
        assert!(refs("costs 100$ total").is_empty());
    }

    #[test]
    fn test_legit_refs_still_parse() {
        assert_eq!(refs("$MY_KEY$"), vec!["MY_KEY".to_string()]);
        assert_eq!(refs("$MY_KEY|Y$"), vec!["MY_KEY".to_string()]);
        assert_eq!(
            refs("$military_industrial_organization_funds_gain$"),
            vec!["military_industrial_organization_funds_gain".to_string()]
        );
    }
}
