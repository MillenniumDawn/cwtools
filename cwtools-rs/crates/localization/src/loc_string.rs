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
            _ => {
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
    s[start..]
        .as_bytes()
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

    if content.contains('.') || content.contains('(') {
        if let Ok(commands) = parse_jomini(content) {
            return Some((LocElement::JominiCommand(commands), i));
        }
    }

    let command = content.find('|')
        .map(|p| &content[..p])
        .unwrap_or(content);

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
                    commands.push(JominiCommand { key: std::mem::take(&mut current), params: Vec::new() });
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
        commands.push(JominiCommand { key: current, params: Vec::new() });
    }

    Ok(commands)
}

<<<<<<< Updated upstream
fn parse_jomini_params(
    chars: &mut std::iter::Peekable<std::str::Chars>,
) -> Result<Vec<JominiParam>, String> {
=======
fn parse_jomini_params(chars: &[char], i: &mut usize) -> Result<Vec<JominiParam>, String> {
>>>>>>> Stashed changes
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
    fn test_ref_colour_in_mixed_string() {
        // Colour-suffixed ref inside mixed text
        let elems = parse_loc_elements("Hello $NAME|G$ world");
        assert_eq!(elems[0], LocElement::Chars("Hello ".to_string()));
        assert_eq!(elems[1], LocElement::Ref("NAME".to_string()));
        assert_eq!(elems[2], LocElement::Chars(" world".to_string()));
    }
}
