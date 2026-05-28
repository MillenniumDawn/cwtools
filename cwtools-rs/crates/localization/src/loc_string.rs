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
//! Mirrors F# `LocalisationString.fs` and handles both the original Paradox
//! syntax (`[GetName]`) and the newer Jomini syntax.

use std::collections::HashMap;

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
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        match chars[i] {
            '$' => {
                if let Some((elem, new_i)) = parse_ref(&chars, i) {
                    elements.push(elem);
                    i = new_i;
                } else {
                    // Lone '$' – treat as plain chars until next special
                    let start = i;
                    i += 1;
                    while i < chars.len() && !['$', '[', ']'].contains(&chars[i]) {
                        i += 1;
                    }
                    elements.push(LocElement::Chars(chars[start..i].iter().collect()));
                }
            }
            '[' => {
                if let Some((elem, new_i)) = parse_bracket(&chars, i) {
                    elements.push(elem);
                    i = new_i;
                } else {
                    // Lone '[' – treat as plain chars
                    let start = i;
                    i += 1;
                    while i < chars.len() && !['$', '[', ']'].contains(&chars[i]) {
                        i += 1;
                    }
                    elements.push(LocElement::Chars(chars[start..i].iter().collect()));
                }
            }
            _ => {
                let start = i;
                i += 1;
                while i < chars.len() && !['$', '[', ']'].contains(&chars[i]) {
                    i += 1;
                }
                elements.push(LocElement::Chars(chars[start..i].iter().collect()));
            }
        }
    }

    elements
}

/// Parse a `$ref$` starting at `chars[start]`.
fn parse_ref(chars: &[char], start: usize) -> Option<(LocElement, usize)> {
    // chars[start] == '$'
    let mut i = start + 1;
    let content_start = i;

    while i < chars.len() && chars[i] != '$' {
        i += 1;
    }

    if i >= chars.len() {
        return None; // No closing '$'
    }

    let content = chars[content_start..i].iter().collect::<String>();
    Some((LocElement::Ref(content), i + 1))
}

/// Parse a `[...]` block starting at `chars[start]`.
fn parse_bracket(chars: &[char], start: usize) -> Option<(LocElement, usize)> {
    // chars[start] == '['
    let mut i = start + 1;
    let content_start = i;
    let mut depth = 1;

    while i < chars.len() && depth > 0 {
        match chars[i] {
            '[' => depth += 1,
            ']' => depth -= 1,
            _ => {}
        }
        i += 1;
    }

    if depth != 0 {
        return None; // Unmatched bracket
    }

    // i now points one past the closing ']'
    let content = chars[content_start..i - 1].iter().collect::<String>();

    // Check if it's Jomini syntax (contains '.')
    if content.contains('.') || content.contains('(') {
        if let Ok(commands) = parse_jomini(&content) {
            return Some((LocElement::JominiCommand(commands), i));
        }
    }

    // Simple command: key, optionally with |
    let command = if let Some(pipe) = content.find('|') {
        content[..pipe].to_string()
    } else {
        content
    };

    Some((LocElement::Command(command), i))
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
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        match chars[i] {
            '.' => {
                if !current.is_empty() {
                    commands.push(JominiCommand {
                        key: current.clone(),
                        params: Vec::new(),
                    });
                    current.clear();
                }
                i += 1;
            }
            '(' => {
                // Function call: key(params)
                let key = current.clone();
                current.clear();
                i += 1; // skip '('
                let params = parse_jomini_params(&chars, &mut i)?;
                commands.push(JominiCommand { key, params });
            }
            ' ' | ',' => {
                i += 1;
            }
            _ => {
                current.push(chars[i]);
                i += 1;
            }
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
    chars: &[char],
    i: &mut usize,
) -> Result<Vec<JominiParam>, String> {
    let mut params = Vec::new();
    let mut current = String::new();

    while *i < chars.len() {
        match chars[*i] {
            ')' => {
                *i += 1;
                if !current.trim().is_empty() {
                    let param = parse_jomini_param(&current)?;
                    params.push(param);
                }
                return Ok(params);
            }
            ',' => {
                *i += 1;
                if !current.trim().is_empty() {
                    let param = parse_jomini_param(&current)?;
                    params.push(param);
                }
                current.clear();
            }
            _ => {
                current.push(chars[*i]);
                *i += 1;
            }
        }
    }

    Err("unclosed parenthesis in Jomini function".to_string())
}

fn parse_jomini_param(s: &str) -> Result<JominiParam, String> {
    let trimmed = s.trim();
    if trimmed.starts_with('\'') && trimmed.ends_with('\'') {
        Ok(JominiParam::Literal(trimmed[1..trimmed.len() - 1].to_string()))
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
        assert_eq!(elems[0], LocElement::Command("event_target:foo".to_string()));
    }

    #[test]
    fn test_question_variable() {
        let elems = parse_loc_elements("[?var_name]");
        assert_eq!(elems.len(), 1);
        assert_eq!(elems[0], LocElement::Command("?var_name".to_string()));
    }
}
