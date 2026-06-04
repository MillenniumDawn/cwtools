use crate::ast::*;
use cwtools_string_table::string_table::{StringTable, StringTokens};
use std::str::Chars;

#[allow(dead_code)]
struct Parser<'a> {
    input: &'a str,
    chars: Chars<'a>,
    line: u32,
    col: u16,
    table: &'a StringTable,
    arena: Arena,
    errors: Vec<ParseError>,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str, table: &'a StringTable) -> Self {
        Self {
            input,
            chars: input.chars(),
            line: 1,
            col: 0,
            table,
            arena: Arena::new(),
            errors: Vec::new(),
        }
    }

    fn pos(&self) -> SourcePos {
        SourcePos {
            line: self.line,
            col: self.col,
        }
    }

    fn advance(&mut self) -> Option<char> {
        let c = self.chars.next()?;
        if c == '\n' {
            self.line += 1;
            self.col = 0;
        } else {
            self.col += 1;
        }
        Some(c)
    }

    fn peek(&self) -> Option<char> {
        self.chars.clone().next()
    }

    fn skip_whitespace(&mut self) {
        while let Some(c) = self.peek() {
            if c.is_whitespace() {
                self.advance();
            } else {
                break;
            }
        }
    }

    fn consume_comment(&mut self) -> Option<Comment> {
        if self.peek() == Some('#') {
            let start = self.pos();
            // Do NOT consume the '#'; keep it in the comment text so that
            // directive comments like '## cardinality = ...' remain intact.
            let mut text = String::new();
            while let Some(c) = self.peek() {
                if c == '\n' {
                    break;
                }
                text.push(c);
                self.advance();
            }
            let end = self.pos();
            return Some(Comment {
                text,
                pos: SourceRange { start, end },
            });
        }
        None
    }

    fn parse_operator(&mut self) -> Option<Operator> {
        let ahead: String = self.chars.clone().take(2).collect();
        let op = match ahead.as_str() {
            "<=" => Some(Operator::LessThanOrEqual),
            ">=" => Some(Operator::GreaterThanOrEqual),
            "!=" => Some(Operator::NotEqual),
            "==" => Some(Operator::EqualEqual),
            "?=" => Some(Operator::QuestionEqual),
            _ => {
                let c = self.peek()?;
                match c {
                    '=' => Some(Operator::Equals),
                    '>' => Some(Operator::GreaterThan),
                    '<' => Some(Operator::LessThan),
                    _ => None,
                }
            }
        };

        if let Some(ref o) = op {
            let len = o.as_str().len();
            for _ in 0..len {
                self.advance();
            }
            self.skip_whitespace();
        }
        op
    }

    fn parse_key(&mut self) -> Option<StringTokens> {
        if self.peek() == Some('"') {
            // Quoted key
            self.advance();
            let mut s = String::new();
            while let Some(c) = self.peek() {
                if c == '\\' {
                    self.advance();
                    if let Some(escaped) = self.advance() {
                        s.push(escaped);
                        continue;
                    }
                } else if c == '"' {
                    self.advance();
                    break;
                }
                s.push(c);
                self.advance();
            }
            self.skip_whitespace();
            Some(self.table.intern(&format!("\"{}\"", s)))
        } else {
            let mut s = String::new();
            while let Some(c) = self.peek() {
                if c.is_alphanumeric()
                    || c == '_'
                    || c == ':'
                    || c == '@'
                    || c == '.'
                    || c == '\"'
                    || c == '-'
                    || c == '\''
                    || c == '['
                    || c == ']'
                    || c == '!'
                    || c == '<'
                    || c == '>'
                    || c == '$'
                    || c == '^'
                    || c == '&'
                    || c == '|'
                    || c == '('
                    || c == ')'
                {
                    s.push(c);
                    self.advance();
                } else {
                    break;
                }
            }
            if s.is_empty() {
                return None;
            }
            self.skip_whitespace();
            Some(self.table.intern(&s))
        }
    }

    fn parse_value(&mut self) -> Option<Value> {
        self.skip_whitespace();
        // Skip any comments that appear before the actual value (e.g. value on next line).
        // The AST has no place for comments inside Leaf values, so just discard them.
        while self.consume_comment().is_some() {
            self.skip_whitespace();
        }

        if self.peek() == Some('{') {
            return self.parse_clause();
        }

        if self.peek() == Some('"') {
            self.advance();
            let mut s = String::new();
            while let Some(c) = self.peek() {
                if c == '\\' {
                    self.advance();
                    if let Some(escaped) = self.advance() {
                        let translated = match escaped {
                            'n' => '\n',
                            't' => '\t',
                            'r' => '\r',
                            '\\' => '\\',
                            '"' => '"',
                            _ => escaped,
                        };
                        s.push(translated);
                        continue;
                    }
                } else if c == '"' {
                    self.advance();
                    break;
                }
                s.push(c);
                self.advance();
            }
            self.skip_whitespace();
            let tokens = self.table.intern(&format!("\"{}\"", s));
            return Some(Value::QString(tokens));
        }

        // Peek ahead for numbers / booleans / rgb / hsv / metaprogramming
        let ahead: String = self.chars.clone().take(64).collect();
        let trimmed = ahead.trim();

        // rgb / hsv detection.
        // Determine the candidate keyword ("rgb", "rgb360", "hsv", "hsv360") and
        // only proceed when the char after the keyword is absent or non-alphanumeric,
        // so that identifiers like `rgbx` or `rgb3foo` are excluded.
        // We save state and restore it if parse_rgb/parse_hsv returns None, so a
        // bare `rgb` token that isn't followed by `{` doesn't get consumed and lost.
        let is_rgb = trimmed.starts_with("rgb") || trimmed.starts_with("RGB");
        let is_hsv = trimmed.starts_with("hsv") || trimmed.starts_with("HSV");
        if is_rgb {
            // Determine keyword length: "rgb360" (6) or "rgb" (3)
            let kw_len = if trimmed[3..].starts_with("360") { 6 } else { 3 };
            let after = trimmed.as_bytes().get(kw_len).map(|&b| b as char);
            if after.map_or(true, |c| !c.is_alphanumeric()) {
                let saved = self.pos();
                let saved_chars = self.chars.clone();
                if let Some(v) = self.parse_rgb() {
                    return Some(v);
                }
                self.chars = saved_chars;
                self.line = saved.line;
                self.col = saved.col;
            }
        }
        if is_hsv {
            let kw_len = if trimmed[3..].starts_with("360") { 6 } else { 3 };
            let after = trimmed.as_bytes().get(kw_len).map(|&b| b as char);
            if after.map_or(true, |c| !c.is_alphanumeric()) {
                let saved = self.pos();
                let saved_chars = self.chars.clone();
                if let Some(v) = self.parse_hsv() {
                    return Some(v);
                }
                self.chars = saved_chars;
                self.line = saved.line;
                self.col = saved.col;
            }
        }
        if trimmed.starts_with("yes") {
            let saved = self.pos();
            let saved_chars = self.chars.clone();
            for _ in 0..3 { self.advance(); }
            if let Some(c) = self.peek() {
                if !is_value_char(c) {
                    self.skip_whitespace();
                    return Some(Value::Bool(true));
                }
            } else {
                self.skip_whitespace();
                return Some(Value::Bool(true));
            }
            // Not a standalone "yes" — backtrack
            self.chars = saved_chars;
            self.line = saved.line;
            self.col = saved.col;
        }
        if trimmed.starts_with("no") {
            let saved = self.pos();
            let saved_chars = self.chars.clone();
            for _ in 0..2 { self.advance(); }
            if let Some(c) = self.peek() {
                if !is_value_char(c) {
                    self.skip_whitespace();
                    return Some(Value::Bool(false));
                }
            } else {
                self.skip_whitespace();
                return Some(Value::Bool(false));
            }
            // Not a standalone "no" — backtrack
            self.chars = saved_chars;
            self.line = saved.line;
            self.col = saved.col;
        }
        if trimmed.starts_with("@[") {
            return self.parse_metaprogramming();
        }

        // Try integer then float
        let mut num_str = String::new();
        let mut had_dot = false;
        if let Some('-') = self.peek() {
            num_str.push('-');
            self.advance();
        }
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                num_str.push(c);
                self.advance();
            } else if c == '.' && !had_dot {
                had_dot = true;
                num_str.push(c);
                self.advance();
            } else {
                break;
            }
        }
        if !num_str.is_empty() && num_str != "-" {
            if let Ok(i) = num_str.parse::<i64>() {
                if let Some(c) = self.peek() {
                    if !is_value_char(c) {
                        self.skip_whitespace();
                        return Some(Value::Int(i));
                    }
                } else {
                    self.skip_whitespace();
                    return Some(Value::Int(i));
                }
            }
            if let Ok(f) = num_str.parse::<f64>() {
                if let Some(c) = self.peek() {
                    if !is_value_char(c) {
                        self.skip_whitespace();
                        return Some(Value::Float(f));
                    }
                } else {
                    self.skip_whitespace();
                    return Some(Value::Float(f));
                }
            }
        }

        // Fallback: plain string
        let mut s = String::new();
        while let Some(c) = self.peek() {
            if is_value_char(c) {
                s.push(c);
                self.advance();
            } else {
                break;
            }
        }
        if s.is_empty() {
            return None;
        }
        self.skip_whitespace();
        let tokens = self.table.intern(&s);
        Some(Value::String(tokens))
    }

    fn parse_clause(&mut self) -> Option<Value> {
        if self.peek() != Some('{') {
            return None;
        }
        self.advance(); // '{'
        self.skip_whitespace();

        let mut children = Vec::new();
        loop {
            self.skip_whitespace();
            if self.peek() == Some('}') {
                self.advance();
                self.skip_whitespace();
                break;
            }
            if self.peek().is_none() {
                // Unclosed brace — record error and break to avoid infinite loop
                self.errors.push(ParseError::Pos(
                    "".to_string(),
                    self.pos().line,
                    self.pos().col,
                    "unclosed clause: expected '}' before end of file".to_string(),
                ));
                break;
            }
            self.parse_statement(&mut children);
        }

        Some(Value::Clause(children))
    }

    fn parse_statement(&mut self, out: &mut Vec<Child>) {
        self.skip_whitespace();

        if let Some(comment) = self.consume_comment() {
            let idx = self.arena.push_comment(comment);
            out.push(Child::Comment(idx));
            return;
        }

        // Try key=value (or key { ... } shorthand)
        let saved = self.pos();
        let saved_chars = self.chars.clone();
        if let Some(key) = self.parse_key() {
            if let Some(op) = self.parse_operator() {
                if let Some(value) = self.parse_value() {
                    let end = self.pos();
                    let leaf = Leaf {
                        key,
                        value,
                        op,
                        pos: SourceRange { start: saved, end },
                    };
                    let idx = self.arena.push_leaf(leaf);
                    out.push(Child::Leaf(idx));
                    return;
                }
                // key = EOF  — commit as error instead of backtracking
                let end = self.pos();
                let leaf = Leaf {
                    key,
                    value: Value::String(self.table.intern("")),
                    op,
                    pos: SourceRange { start: saved, end },
                };
                let idx = self.arena.push_leaf(leaf);
                out.push(Child::Leaf(idx));
                self.errors.push(ParseError::Pos(
                    "".to_string(),
                    saved.line,
                    saved.col,
                    format!("key '{}' has no value after '='", self.table.get_string(key.normal).unwrap_or_default()),
                ));
                return;
            }
            // No operator — check for shorthand `key { ... }`
            self.skip_whitespace();
            if let Some('{') = self.peek() {
                if let Some(value) = self.parse_clause() {
                    let end = self.pos();
                    let leaf = Leaf {
                        key,
                        value,
                        op: Operator::Equals,
                        pos: SourceRange { start: saved, end },
                    };
                    let idx = self.arena.push_leaf(leaf);
                    out.push(Child::Leaf(idx));
                    return;
                }
            }
            // Not a key=value or shorthand; restore and try leaf-value
            self.chars = saved_chars;
            self.line = saved.line;
            self.col = saved.col;
        }

        // Leaf value (bare value)
        if let Some(value) = self.parse_value() {
            let end = self.pos();
            let lv = LeafValue {
                value,
                pos: SourceRange { start: saved, end },
            };
            let idx = self.arena.push_leaf_value(lv);
            out.push(Child::LeafValue(idx));
            return;
        }

        // Nothing matched — consume one char to avoid infinite loop on malformed input
        self.advance();
    }

    fn parse_rgb(&mut self) -> Option<Value> {
        // "rgb" is already peeked and matched in parse_value before this is called.
        // Consume "rgb" or "RGB" and optional "360".
        for _ in 0..3 { self.advance(); }
        self.skip_whitespace();
        if let Some('3') = self.peek() {
            let ahead: String = self.chars.clone().take(3).collect();
            if ahead == "360" {
                self.advance();
                self.advance();
                self.advance();
                self.skip_whitespace();
            }
        }
        // Support `rgb = { ... }` by skipping optional '='
        if self.peek() == Some('=') {
            self.advance();
            self.skip_whitespace();
        }
        self.parse_clause()
    }

    fn parse_hsv(&mut self) -> Option<Value> {
        for _ in 0..3 { self.advance(); }
        self.skip_whitespace();
        if let Some('3') = self.peek() {
            let ahead: String = self.chars.clone().take(3).collect();
            if ahead == "360" {
                self.advance();
                self.advance();
                self.advance();
                self.skip_whitespace();
            }
        }
        // Support `hsv = { ... }` by skipping optional '='
        if self.peek() == Some('=') {
            self.advance();
            self.skip_whitespace();
        }
        self.parse_clause()
    }

    fn parse_metaprogramming(&mut self) -> Option<Value> {
        // Must start with "@["
        if self.peek() != Some('@') {
            return None;
        }
        self.advance(); // '@'
        if self.peek() != Some('[') {
            return None;
        }
        self.advance(); // '['

        let mut s = String::new();
        s.push_str("@[");
        let mut found_close = false;
        while let Some(c) = self.peek() {
            if c == ']' {
                s.push(c);
                self.advance();
                found_close = true;
                break;
            }
            s.push(c);
            self.advance();
        }
        if !found_close {
            self.errors.push(ParseError::Pos(
                "".to_string(),
                self.pos().line,
                self.pos().col,
                "unclosed metaprogramming bracket: expected ']'".to_string(),
            ));
            return None;
        }
        self.skip_whitespace();
        let tokens = self.table.intern(&s);
        Some(Value::String(tokens))
    }

    fn parse(mut self) -> Result<ParsedFile, ParseError> {
        self.skip_whitespace();
        let mut root_children = Vec::new();
        while self.peek().is_some() {
            self.parse_statement(&mut root_children);
        }
        Ok(ParsedFile {
            arena: self.arena,
            root_children,
            errors: self.errors,
        })
    }
}

fn is_value_char(c: char) -> bool {
    c.is_alphanumeric()
        || c == '_'
        || c == '.'
        || c == '-'
        || c == ':'
        || c == ';'
        || c == '\''
        || c == '['
        || c == ']'
        || c == '@'
        || c == '+'
        || c == '`'
        || c == '%'
        || c == '/'
        || c == '!'
        || c == ','
        || c == '<'
        || c == '>'
        || c == '?'
        || c == '$'
        || c == '\\'
        || c == 'š'
        || c == 'Š'
        || c == '’'
        || c == '|'
        || c == '^'
        || c == '*'
        || c == '&'
        || c == '('
        || c == ')'
}

/// Strip UTF-8 BOM if present, then parse.
pub fn parse_string(input: &str, table: &StringTable) -> Result<ParsedFile, ParseError> {
    let stripped = input.strip_prefix('\u{FEFF}').unwrap_or(input);
    let parser = Parser::new(stripped, table);
    parser.parse()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_key_value() {
        let table = StringTable::new();
        let result = parse_string("foo = bar", &table).unwrap();
        assert_eq!(result.root_children.len(), 1);
    }

    #[test]
    fn nested_clause() {
        let table = StringTable::new();
        let result = parse_string("root = { a = 1 }", &table).unwrap();
        assert_eq!(result.root_children.len(), 1);
    }

    #[test]
    fn parse_real_file() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../artifacts/bin/CWToolsTests/debug/testfiles/performancetest2/common/static_modifiers/cc_colony_events_static_modifiers.txt"
        );
        let input = std::fs::read_to_string(path).unwrap();
        let table = StringTable::new();
        let result = parse_string(&input, &table).unwrap();
        assert!(!result.root_children.is_empty());
        assert!(table.len() > 0);
    }

    #[test]
    fn parse_angle_bracket_value() {
        let table = StringTable::new();
        let result = parse_string("ethos = <ethos>", &table).unwrap();
        assert_eq!(result.root_children.len(), 1);
        if let Child::Leaf(idx) = &result.root_children[0] {
            let leaf = &result.arena.leaves[*idx as usize];
            let key = table.get_string(leaf.key.normal).unwrap_or_default();
            assert_eq!(key, "ethos");
            let val = match &leaf.value {
                Value::String(t) | Value::QString(t) => table.get_string(t.normal).unwrap_or_default(),
                _ => panic!("expected string value, got {:?}", leaf.value),
            };
            assert_eq!(val, "<ethos>");
        } else {
            panic!("expected leaf child");
        }
    }
}
