use crate::ast::*;
use cwtools_string_table::string_table::{StringTable, StringTokens};
use std::str::Chars;

struct Parser<'a> {
    input: &'a str,
    chars: Chars<'a>,
    line: u32,
    col: u16,
    table: &'a StringTable,
    arena: Arena,
    errors: Vec<ParseError>,
}

/// Saved cursor for backtracking: the remaining-input iterator plus line/col.
#[derive(Clone)]
struct Cursor<'a> {
    chars: Chars<'a>,
    line: u32,
    col: u16,
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

    /// Byte offset of the cursor into the original input.
    fn byte_pos(&self) -> usize {
        self.input.len() - self.chars.as_str().len()
    }

    fn save(&self) -> Cursor<'a> {
        Cursor {
            chars: self.chars.clone(),
            line: self.line,
            col: self.col,
        }
    }

    fn restore(&mut self, c: Cursor<'a>) {
        self.chars = c.chars;
        self.line = c.line;
        self.col = c.col;
    }

    fn advance(&mut self) -> Option<char> {
        let c = self.chars.next()?;
        if c == '\n' {
            self.line += 1;
            self.col = 0;
        } else if c != '\r' {
            // '\r' is not counted toward column (CRLF line endings: \r\n is one newline).
            self.col += 1;
        }
        Some(c)
    }

    fn peek(&self) -> Option<char> {
        self.chars.clone().next()
    }

    /// Peek at the second character without consuming anything.
    fn peek2(&self) -> Option<char> {
        let mut it = self.chars.clone();
        it.next();
        it.next()
    }

    /// Collect up to `N` upcoming chars into a stack-allocated buffer without
    /// advancing the iterator. Returns the actual number of chars written.
    fn peek_n<const N: usize>(&self) -> ([char; N], usize) {
        let mut buf = ['\0'; N];
        let mut count = 0;
        for c in self.chars.clone().take(N) {
            buf[count] = c;
            count += 1;
        }
        (buf, count)
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
        let c1 = self.peek()?;
        let c2 = self.peek2();
        let op = match (c1, c2) {
            ('<', Some('=')) => Some(Operator::LessThanOrEqual),
            ('>', Some('=')) => Some(Operator::GreaterThanOrEqual),
            ('!', Some('=')) => Some(Operator::NotEqual),
            ('=', Some('=')) => Some(Operator::EqualEqual),
            ('?', Some('=')) => Some(Operator::QuestionEqual),
            ('=', _) => Some(Operator::Equals),
            ('>', _) => Some(Operator::GreaterThan),
            ('<', _) => Some(Operator::LessThan),
            _ => None,
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
            // Quoted key — same escape rules as quoted values (F# SharedParsers.fs:183-186).
            self.advance();
            // Build with surrounding quotes directly to avoid a format!("\"{}\"", s) copy.
            let mut s = String::from('"');
            while let Some(c) = self.peek() {
                if c == '\\' {
                    self.advance(); // consume '\'
                    match self.peek() {
                        Some('"') => {
                            self.advance();
                            s.push('"');
                        }
                        Some('\\') => {
                            self.advance();
                            s.push('\\');
                        }
                        _ => {
                            // Keep literal backslash; next char picked up naturally.
                            s.push('\\');
                        }
                    }
                    continue;
                } else if c == '"' {
                    self.advance();
                    break;
                }
                s.push(c);
                self.advance();
            }
            s.push('"');
            self.skip_whitespace();
            Some(self.table.intern(&s))
        } else {
            let start = self.byte_pos();
            while let Some(c) = self.peek() {
                // A `?` belongs to the key when it is a `?<default>` null-coalescing
                // selector (`my_var?150 = ...`, the TAOG form), but NOT when it is
                // the `?=` QuestionEqual operator — stop before `?=` so the operator
                // still lexes. `^` carries no such ambiguity (no `^=` operator).
                if c == '?' && self.peek2() == Some('=') {
                    break;
                }
                if is_key_char(c) {
                    self.advance();
                } else {
                    break;
                }
            }
            let s = &self.input[start..self.byte_pos()];
            if s.is_empty() {
                return None;
            }
            self.skip_whitespace();
            Some(self.table.intern(s))
        }
    }

    /// Parse a value. `leafvalue` is true when the value is a bare value in a
    /// clause (e.g. a namelist name), false when it is the RHS of `key = value`.
    /// Only leafvalues may carry interior quotes; a key's RHS quoted string
    /// closes strictly at the first `"` so that one-line `a = "x" b = "y"` pairs
    /// (history/units, version_name, ...) parse as separate statements.
    fn parse_value(&mut self, leafvalue: bool) -> Option<Value> {
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
            let quote_start = self.pos();
            self.advance();
            // Build with surrounding quotes directly to avoid a format!("\"{}\"", s) copy.
            let mut s = String::from('"');
            let mut closed = false;
            while let Some(c) = self.peek() {
                if c == '\n' {
                    // Never span lines in a quoted string. For a key-RHS string
                    // (`a = “oops`) this is an unclosed quote; for a leafvalue
                    // the interior-quote heuristic stays single-line already
                    // (this matches the prior `leafvalue && c == '\n'` behavior
                    // and extends it to key-RHS, fixing spec item 3.5).
                    break;
                }
                if c == '\\' {
                    self.advance(); // consume '\'
                    match self.peek() {
                        Some('"') => {
                            // \" -> " (unescape)
                            self.advance();
                            s.push('"');
                        }
                        Some('\\') => {
                            // \\ -> \ (unescape)
                            self.advance();
                            s.push('\\');
                        }
                        _ => {
                            // Any other \X: keep the backslash and let the loop
                            // pick up the next char naturally (matches F# behaviour).
                            s.push('\\');
                        }
                    }
                    continue;
                } else if c == '"' {
                    if !leafvalue {
                        // Key-RHS quoted string: `"` always closes (strict). Keeps
                        // one-line `a = "x" b = "y"` as separate statements.
                        self.advance();
                        closed = true;
                        break;
                    }
                    // Decide: real terminator or interior literal quote. Paradox
                    // namelists embed quotes inside a name, e.g.
                    //   1 = { "Division "Castillejos"" }
                    //   2 = { "Grupo de Operaciones Especiales "Granada" II" }
                    // Rules, applied to the just-closed `"`:
                    //   - immediately followed by another `"` (no gap): an interior
                    //     `""` pair — keep one literal quote, let the next `"` be
                    //     judged on its own.
                    //   - otherwise look past whitespace to the next significant
                    //     char: a delimiter (`{ } = #`), another opening `"`, or EOF
                    //     closes the string; bare content means this `"` was an
                    //     interior quote and the name continues.
                    // This intentionally diverges from F#/Clausewitz (which split the
                    // name at the first interior quote) so the Rust engine accepts
                    // all valid game configuration as the F# engine is retired.
                    self.advance(); // consume the `"`
                    if self.peek() == Some('"') {
                        s.push('"');
                        continue;
                    }
                    // Look past same-line whitespace (space/tab/CR) ONLY. A newline
                    // always ends the string: interior quotes appear within a single
                    // line, so the close `"` of a normal `key = "value"` followed by a
                    // newline must terminate (not swallow the next line).
                    let next_sig = {
                        let mut it = self.chars.clone();
                        let mut nc = it.next();
                        while matches!(nc, Some(' ') | Some('\t') | Some('\r')) {
                            nc = it.next();
                        }
                        nc
                    };
                    match next_sig {
                        None | Some('\n') | Some('}') | Some('{') | Some('=') | Some('#')
                        | Some('"') => break,
                        _ => {
                            s.push('"');
                            continue;
                        }
                    }
                }
                s.push(c);
                self.advance();
            }
            if !leafvalue && !closed {
                self.errors.push(ParseError::Pos(
                    "".to_string(),
                    quote_start.line,
                    quote_start.col,
                    format!(
                        "unclosed quoted string starting at line {}",
                        quote_start.line
                    ),
                ));
            }
            s.push('"');
            self.skip_whitespace();
            let tokens = self.table.intern(&s);
            return Some(Value::QString(tokens));
        }

        // Peek ahead for numbers / booleans / rgb / hsv / metaprogramming
        // rgb / hsv detection.
        // Determine the candidate keyword ("rgb", "rgb360", "hsv", "hsv360") and
        // only proceed when the char after the keyword is absent or non-alphanumeric,
        // so that identifiers like `rgbx` or `rgb3foo` are excluded.
        // We save state and restore it if parse_rgb/parse_hsv returns None, so a
        // bare `rgb` token that isn't followed by `{` doesn't get consumed and lost.
        // Peek 7 chars (max needed: "rgb360" + one char after) without allocating.
        // Only the keywords rgb/hsv/yes/no and the metaprogramming prefix start
        // with one of these chars, so plain numbers/identifiers skip the multi-char
        // peek entirely.
        let (peek7, peek7_len) = match self.peek() {
            Some('r' | 'R' | 'h' | 'H' | 'y' | 'n' | '@') => self.peek_n::<7>(),
            _ => (['\0'; 7], 0),
        };
        let is_rgb = peek7_len >= 3
            && peek7[0].eq_ignore_ascii_case(&'r')
            && peek7[1].eq_ignore_ascii_case(&'g')
            && peek7[2].eq_ignore_ascii_case(&'b');
        let is_hsv = peek7_len >= 3
            && peek7[0].eq_ignore_ascii_case(&'h')
            && peek7[1].eq_ignore_ascii_case(&'s')
            && peek7[2].eq_ignore_ascii_case(&'v');
        if is_rgb {
            let kw_len = if peek7_len >= 6 && peek7[3] == '3' && peek7[4] == '6' && peek7[5] == '0'
            {
                6
            } else {
                3
            };
            let after = if peek7_len > kw_len {
                Some(peek7[kw_len])
            } else {
                None
            };
            if after.is_none_or(|c| !c.is_alphanumeric()) {
                let saved = self.save();
                if let Some(v) = self.parse_color_clause() {
                    return Some(v);
                }
                self.restore(saved);
            }
        }
        if is_hsv {
            let kw_len = if peek7_len >= 6 && peek7[3] == '3' && peek7[4] == '6' && peek7[5] == '0'
            {
                6
            } else {
                3
            };
            let after = if peek7_len > kw_len {
                Some(peek7[kw_len])
            } else {
                None
            };
            if after.is_none_or(|c| !c.is_alphanumeric()) {
                let saved = self.save();
                if let Some(v) = self.parse_color_clause() {
                    return Some(v);
                }
                self.restore(saved);
            }
        }
        if peek7_len >= 3 && peek7[0] == 'y' && peek7[1] == 'e' && peek7[2] == 's' {
            let saved = self.save();
            for _ in 0..3 {
                self.advance();
            }
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
            self.restore(saved);
        }
        if peek7_len >= 2 && peek7[0] == 'n' && peek7[1] == 'o' {
            let saved = self.save();
            for _ in 0..2 {
                self.advance();
            }
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
            self.restore(saved);
        }
        // F# metaprogramming prefix is "@\[" (at, backslash, open-bracket).
        // SharedParsers.fs:244 uses pstring "@\\[" which is the 3-char literal @\[.
        if peek7_len >= 3 && peek7[0] == '@' && peek7[1] == '\\' && peek7[2] == '[' {
            return self.parse_metaprogramming();
        }

        // Try integer then float.
        //
        // F# uses `attempt valueInt` then `attempt valueFloat` with backtracking.
        // If parsing a numeric literal fails (e.g. "1444.11.11", "1e5", "0x1A"),
        // FParsec backtracks to the start and valueStr consumes the whole token.
        //
        // We replicate this: save position, scan digits+optional-dot, then check
        // that the NEXT char is NOT a value-char (i.e. the token ends here).  If
        // it is still a value-char we must backtrack and let the fallback string
        // path consume the entire token.
        //
        // Leading '+' is accepted by F#'s pint64/pfloat (issue #2).
        let num_saved = self.save();

        let mut num_str = String::new();
        let mut had_dot = false;

        match self.peek() {
            Some('-') | Some('+') => {
                let sign = self.peek().unwrap();
                num_str.push(sign);
                self.advance();
            }
            _ => {}
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

        // Only commit a numeric result when the token ends here (next char is not
        // a value-char).  Otherwise backtrack so the whole token becomes a String.
        let num_token_ends = match self.peek() {
            None => true,
            Some(c) => !is_value_char(c),
        };

        if num_token_ends && !num_str.is_empty() && num_str != "-" && num_str != "+" {
            // Try int first (strips leading '+' via parse::<i64>)
            if let Ok(i) = num_str.parse::<i64>() {
                self.skip_whitespace();
                return Some(Value::Int(i));
            }
            if let Ok(f) = num_str.parse::<f64>() {
                self.skip_whitespace();
                return Some(Value::Float(f));
            }
        }

        // Numeric parse didn't commit — backtrack fully so the string path gets the
        // whole token (e.g. "1444.11.11", "1e5", "0x1A", lone "-").
        self.restore(num_saved);

        // Fallback: plain string
        let start = self.byte_pos();
        while let Some(c) = self.peek() {
            if is_value_char(c) {
                self.advance();
            } else {
                break;
            }
        }
        let s = &self.input[start..self.byte_pos()];
        if s.is_empty() {
            return None;
        }
        self.skip_whitespace();
        let tokens = self.table.intern(s);
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
        let saved_cursor = self.save();
        if let Some(key) = self.parse_key() {
            if let Some(op) = self.parse_operator() {
                if let Some(value) = self.parse_value(false) {
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
                    format!(
                        "key '{}' has no value after '='",
                        self.table.get_string(key.normal).unwrap_or_default()
                    ),
                ));
                return;
            }
            // No operator — check for shorthand `key { ... }`
            self.skip_whitespace();
            if let Some('{') = self.peek()
                && let Some(value) = self.parse_clause()
            {
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
            // Not a key=value or shorthand; restore and try leaf-value
            self.restore(saved_cursor);
        }

        // Leaf value (bare value)
        if let Some(value) = self.parse_value(true) {
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

    /// Parse a color clause after its keyword (`rgb`/`hsv`, already peeked and
    /// matched in `parse_value`). Consumes the 3-char keyword, an optional `360`
    /// suffix, an optional `=`, then the `{ ... }` clause.
    fn parse_color_clause(&mut self) -> Option<Value> {
        for _ in 0..3 {
            self.advance();
        }
        self.skip_whitespace();
        if self.peek() == Some('3') {
            let (p3, _) = self.peek_n::<3>();
            if p3 == ['3', '6', '0'] {
                self.advance();
                self.advance();
                self.advance();
                self.skip_whitespace();
            }
        }
        // Support `rgb = { ... }` / `hsv = { ... }` by skipping optional '='.
        if self.peek() == Some('=') {
            self.advance();
            self.skip_whitespace();
        }
        self.parse_clause()
    }

    fn parse_metaprogramming(&mut self) -> Option<Value> {
        // F# prefix is "@\[" (at, backslash, open-bracket) — SharedParsers.fs:244.
        // metaprogrammingCharSnippet accepts everything except ']' and '\'.
        // The closing char is ']' (consumed by `ch ']'`).
        // Result token includes the prefix "@\[" and the closing ']'.
        if self.peek() != Some('@') {
            return None;
        }
        let start = self.byte_pos();
        self.advance(); // '@'
        if self.peek() != Some('\\') {
            return None;
        }
        self.advance(); // '\'
        if self.peek() != Some('[') {
            return None;
        }
        self.advance(); // '['

        let mut found_close = false;
        while let Some(c) = self.peek() {
            if c == ']' {
                self.advance();
                found_close = true;
                break;
            }
            // F# metaprogrammingCharSnippet stops at '\' too — just collect content.
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
        let s = &self.input[start..self.byte_pos()];
        self.skip_whitespace();
        let tokens = self.table.intern(s);
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

/// Build a `[bool; 128]` ASCII class table at compile time: every ASCII
/// alphanumeric plus the explicit punctuation in `extra` is `true`.
const fn ascii_class(extra: &[u8]) -> [bool; 128] {
    let mut table = [false; 128];
    let mut i = 0u8;
    while i < 128 {
        let b = i;
        if b.is_ascii_alphanumeric() {
            table[i as usize] = true;
        }
        i += 1;
    }
    let mut j = 0;
    while j < extra.len() {
        table[extra[j] as usize] = true;
        j += 1;
    }
    table
}

/// ASCII members of the bare-value (leafvalue / value-string) char class.
/// Non-ASCII chars are handled separately in [`is_value_char`].
static VALUE_CHAR: [bool; 128] = ascii_class(b"_.-:;'[]@+`%/!,<>?$\\|^*&()");

/// ASCII members of the unquoted-key char class. Non-ASCII alphanumerics are
/// handled separately in [`is_key_char`]. A `?` is included for the
/// `my_var?<default>` null-coalescing selector; `parse_key` separately stops
/// before a `?=` so the QuestionEqual operator still lexes.
static KEY_CHAR: [bool; 128] = ascii_class(b"_:@.\"-'[]!<>$^&|()?");

fn is_value_char(c: char) -> bool {
    if c.is_ascii() {
        VALUE_CHAR[c as usize]
    } else {
        c.is_alphanumeric() || c == 'š' || c == 'Š' || c == '’'
    }
}

fn is_key_char(c: char) -> bool {
    if c.is_ascii() {
        KEY_CHAR[c as usize]
    } else {
        c.is_alphanumeric()
    }
}

/// Strip UTF-8 BOM if present, then parse.
#[tracing::instrument(skip_all)]
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
            "/../../testfiles/performancetest2/common/static_modifiers/cc_colony_events_static_modifiers.txt"
        );
        let input = std::fs::read_to_string(path).unwrap();
        let table = StringTable::new();
        let result = parse_string(&input, &table).unwrap();
        assert!(!result.root_children.is_empty());
        assert!(!table.is_empty());
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
                Value::String(t) | Value::QString(t) => {
                    table.get_string(t.normal).unwrap_or_default()
                }
                _ => panic!("expected string value, got {:?}", leaf.value),
            };
            assert_eq!(val, "<ethos>");
        } else {
            panic!("expected leaf child");
        }
    }

    // -----------------------------------------------------------------------
    // Issue 1: Numeric token corruption — tokens like "1444.11.11" must not
    // be split; the whole thing should become a String.
    // -----------------------------------------------------------------------

    fn value_of(result: &ParsedFile, _table: &StringTable, idx: usize) -> Value {
        match &result.root_children[idx] {
            Child::Leaf(i) => result.arena.leaves[*i as usize].value.clone(),
            Child::LeafValue(i) => result.arena.leaf_values[*i as usize].value.clone(),
            _ => panic!("unexpected child kind"),
        }
    }

    #[test]
    fn date_token_is_string() {
        // "1444.11.11" must parse as one String, not be split at the first dot.
        let table = StringTable::new();
        let result = parse_string("start = 1444.11.11", &table).unwrap();
        match value_of(&result, &table, 0) {
            Value::String(t) => {
                assert_eq!(table.get_string(t.normal).unwrap_or_default(), "1444.11.11");
            }
            v => panic!("expected String, got {:?}", v),
        }
    }

    #[test]
    fn normal_float_parses() {
        let table = StringTable::new();
        let result = parse_string("x = 2.75", &table).unwrap();
        match value_of(&result, &table, 0) {
            Value::Float(f) => assert!((f - 2.75).abs() < 1e-9),
            v => panic!("expected Float, got {:?}", v),
        }
    }

    #[test]
    fn hex_like_token_is_string() {
        // "0x1A" — after "0" the 'x' is a value-char so the whole thing is a String.
        let table = StringTable::new();
        let result = parse_string("x = 0x1A", &table).unwrap();
        match value_of(&result, &table, 0) {
            Value::String(t) => {
                assert_eq!(table.get_string(t.normal).unwrap_or_default(), "0x1A");
            }
            v => panic!("expected String, got {:?}", v),
        }
    }

    #[test]
    fn scientific_like_token_is_string() {
        // "1e5" — after "1" the 'e' is a value-char so the whole token is a String.
        let table = StringTable::new();
        let result = parse_string("x = 1e5", &table).unwrap();
        match value_of(&result, &table, 0) {
            Value::String(t) => {
                assert_eq!(table.get_string(t.normal).unwrap_or_default(), "1e5");
            }
            v => panic!("expected String, got {:?}", v),
        }
    }

    // -----------------------------------------------------------------------
    // Issue 2: Leading '+' — "+5" should parse as Int(5).
    // -----------------------------------------------------------------------

    #[test]
    fn leading_plus_parses_as_int() {
        let table = StringTable::new();
        let result = parse_string("x = +5", &table).unwrap();
        match value_of(&result, &table, 0) {
            Value::Int(5) => {}
            v => panic!("expected Int(5), got {:?}", v),
        }
    }

    #[test]
    fn leading_plus_float() {
        let table = StringTable::new();
        let result = parse_string("x = +2.75", &table).unwrap();
        match value_of(&result, &table, 0) {
            Value::Float(f) => assert!((f - 2.75).abs() < 1e-9),
            v => panic!("expected Float, got {:?}", v),
        }
    }

    // -----------------------------------------------------------------------
    // Issue 3: Quoted-string escapes — only \" and \\ are unescaped.
    // \n in source stays as backslash + 'n', not a newline char.
    // Applies equally to quoted keys and quoted values.
    // -----------------------------------------------------------------------

    #[test]
    fn qstr_backslash_n_stays_literal() {
        // Input: x = "hello\nworld"  — \n must NOT become newline
        let table = StringTable::new();
        let result = parse_string(r#"x = "hello\nworld""#, &table).unwrap();
        match value_of(&result, &table, 0) {
            Value::QString(t) => {
                let raw = table.get_string(t.normal).unwrap_or_default();
                // The stored string is wrapped in quotes; strip them
                let inner = raw.trim_matches('"');
                assert!(
                    !inner.contains('\n'),
                    "\\n should stay as two chars, not a newline; got: {:?}",
                    inner
                );
                assert!(inner.contains('\\'), "backslash should be preserved");
            }
            v => panic!("expected QString, got {:?}", v),
        }
    }

    #[test]
    fn qstr_escaped_quote_is_unescaped() {
        // Input: x = "say \"hi\""  — \" becomes "
        // Stored token is wrapped in outer quotes: "say "hi""
        // Use strip_prefix/suffix to remove exactly the outermost quotes.
        let table = StringTable::new();
        let result = parse_string(r#"x = "say \"hi\"""#, &table).unwrap();
        match value_of(&result, &table, 0) {
            Value::QString(t) => {
                let raw = table.get_string(t.normal).unwrap_or_default();
                let inner = raw
                    .strip_prefix('"')
                    .and_then(|s| s.strip_suffix('"'))
                    .unwrap_or(&raw);
                assert_eq!(inner, r#"say "hi""#);
            }
            v => panic!("expected QString, got {:?}", v),
        }
    }

    #[test]
    fn qstr_double_backslash_collapses() {
        // Input: x = "a\\b"  — \\ becomes single \
        let table = StringTable::new();
        let result = parse_string(r#"x = "a\\b""#, &table).unwrap();
        match value_of(&result, &table, 0) {
            Value::QString(t) => {
                let raw = table.get_string(t.normal).unwrap_or_default();
                let inner = raw.trim_matches('"');
                assert_eq!(inner, r"a\b");
            }
            v => panic!("expected QString, got {:?}", v),
        }
    }

    // Helper: the leafvalues (bare values) of `n = { ... }`.
    fn clause_leafvalues(result: &ParsedFile, table: &StringTable) -> Vec<String> {
        let leaf = match &result.root_children[0] {
            Child::Leaf(i) => &result.arena.leaves[*i as usize],
            _ => panic!("expected leaf"),
        };
        let children = match &leaf.value {
            Value::Clause(c) => c,
            v => panic!("expected clause, got {:?}", v),
        };
        children
            .iter()
            .filter_map(|c| match c {
                Child::LeafValue(i) => {
                    let raw = match &result.arena.leaf_values[*i as usize].value {
                        Value::QString(t) | Value::String(t) => {
                            table.get_string(t.normal).unwrap_or_default()
                        }
                        other => panic!("expected string leafvalue, got {:?}", other),
                    };
                    Some(
                        raw.strip_prefix('"')
                            .and_then(|s| s.strip_suffix('"'))
                            .unwrap_or(&raw)
                            .to_string(),
                    )
                }
                _ => None,
            })
            .collect()
    }

    // Paradox namelists embed quotes inside a single name. Each entry must parse
    // as exactly ONE leafvalue with the interior quotes preserved (intentional
    // divergence from F#, which splits the name at the first interior quote).
    #[test]
    fn qstr_interior_quotes_parse_as_single_value() {
        let table = StringTable::new();
        let cases = [
            (
                r#"n = { "Division "Castillejos"" }"#,
                r#"Division "Castillejos""#,
            ),
            (
                r#"n = { "Grupo de Operaciones Especiales "Granada" II" }"#,
                r#"Grupo de Operaciones Especiales "Granada" II"#,
            ),
        ];
        for (input, expected) in cases {
            let result = parse_string(input, &table).unwrap();
            let lvs = clause_leafvalues(&result, &table);
            assert_eq!(lvs.len(), 1, "input {:?} should be ONE leafvalue", input);
            assert_eq!(lvs[0], expected, "input {:?}", input);
        }
    }

    // Whitespace-separated quoted strings must STILL split into separate values
    // (e.g. `division_types = { "light_armor" "medium_armor" }`).
    #[test]
    fn qstr_space_separated_strings_still_split() {
        let table = StringTable::new();
        let result = parse_string(
            r#"n = { "light_armor" "medium_armor" "heavy_armor" }"#,
            &table,
        )
        .unwrap();
        let lvs = clause_leafvalues(&result, &table);
        assert_eq!(
            lvs,
            vec!["light_armor", "medium_armor", "heavy_armor"],
            "space-separated quoted strings must remain separate"
        );
    }

    #[test]
    fn quoted_key_escape_rules_match_value() {
        // "\"key\"" = value — the key's \" should unescape to just "key" without outer quotes
        let table = StringTable::new();
        let result = parse_string(r#""my\"key" = 1"#, &table).unwrap();
        if let Child::Leaf(i) = &result.root_children[0] {
            let leaf = &result.arena.leaves[*i as usize];
            let key_raw = table.get_string(leaf.key.normal).unwrap_or_default();
            // The key is stored as "my"key" (quoted form), inner part is my"key
            assert!(key_raw.contains('"'), "key should contain unescaped quote");
        } else {
            panic!("expected leaf");
        }
    }

    // -----------------------------------------------------------------------
    // Issue 4: CRLF column tracking — '\r' must not advance col.
    // -----------------------------------------------------------------------

    #[test]
    fn crlf_does_not_double_count_column() {
        // Two identical assignments, one with CRLF line ending, one with LF.
        // The column of the second key should be the same in both cases.
        let table = StringTable::new();
        let crlf = parse_string("a = 1\r\nb = 2", &table).unwrap();
        let lf = parse_string("a = 1\nb = 2", &table).unwrap();
        let col_crlf = match &crlf.root_children[1] {
            Child::Leaf(i) => crlf.arena.leaves[*i as usize].pos.start.col,
            _ => panic!(),
        };
        let col_lf = match &lf.root_children[1] {
            Child::Leaf(i) => lf.arena.leaves[*i as usize].pos.start.col,
            _ => panic!(),
        };
        assert_eq!(
            col_crlf, col_lf,
            "CRLF should not skew column (crlf={}, lf={})",
            col_crlf, col_lf
        );
    }

    // -----------------------------------------------------------------------
    // Issue 5: Metaprogramming prefix is "@\[" (at, backslash, bracket).
    // -----------------------------------------------------------------------

    #[test]
    fn metaprogramming_prefix() {
        // "@\[expr]" should parse as a String containing "@\[expr]".
        let table = StringTable::new();
        let input = r"x = @\[expr]";
        let result = parse_string(input, &table).unwrap();
        match value_of(&result, &table, 0) {
            Value::String(t) => {
                let s = table.get_string(t.normal).unwrap_or_default();
                assert_eq!(s, r"@\[expr]");
            }
            v => panic!("expected String, got {:?}", v),
        }
    }

    // -----------------------------------------------------------------------
    // Phase 3: Unclosed key-RHS quoted string must push a parse error and
    // stop at the newline rather than swallowing subsequent statements.
    // -----------------------------------------------------------------------

    #[test]
    fn unclosed_key_rhs_quote_produces_error() {
        let table = StringTable::new();
        // a = "oops has no closing " before the newline.
        // b = 1 must still parse as a separate statement.
        let result = parse_string("a = \"oops\nb = 1", &table).unwrap();
        assert!(
            !result.errors.is_empty(),
            "expected a parse error for the unclosed quoted string"
        );
        assert_eq!(
            result.root_children.len(),
            2,
            "both statements must be present even after an unclosed quote"
        );
    }

    #[test]
    fn unclosed_key_rhs_quote_at_eof_produces_error() {
        let table = StringTable::new();
        let result = parse_string("a = \"unterminated", &table).unwrap();
        assert!(
            !result.errors.is_empty(),
            "expected a parse error for an unclosed string at EOF"
        );
    }

    // A `?<default>` (and `^`) null-coalescing selector on a variable-defining
    // key (`my_var?150 = { ... }`, the TAOG form) must lex as ONE key, not split
    // at the `?` into a bare value + orphaned clause. `^` was already a key char;
    // `?` was not, so the selector form was mis-parsed.

    fn keyed_clause_key(result: &ParsedFile, table: &StringTable, idx: usize) -> String {
        match &result.root_children[idx] {
            Child::Leaf(i) => {
                let leaf = &result.arena.leaves[*i as usize];
                assert!(
                    matches!(leaf.value, Value::Clause(_)),
                    "expected a keyed clause, got {:?}",
                    leaf.value
                );
                table.get_string(leaf.key.normal).unwrap_or_default()
            }
            other => panic!("expected a keyed-clause Leaf, got {:?}", other),
        }
    }

    #[test]
    fn question_selector_key_is_one_keyed_clause() {
        let table = StringTable::new();
        let result =
            parse_string("war_propaganda_decision_cost?150 = { value = 150 }", &table).unwrap();
        assert_eq!(
            result.root_children.len(),
            1,
            "the `?150` selector must not split the key from its clause: {:?}",
            result.root_children
        );
        assert_eq!(
            keyed_clause_key(&result, &table, 0),
            "war_propaganda_decision_cost?150"
        );
    }

    #[test]
    fn question_selector_key_pretaog_leaf_form() {
        // The pre-TAOG `my_var?150 = 100` form: one key=value leaf, not a bare
        // value plus an orphan `= 100`.
        let table = StringTable::new();
        let result = parse_string("war_propaganda_decision_cost?150 = 100", &table).unwrap();
        assert_eq!(result.root_children.len(), 1, "{:?}", result.root_children);
        match &result.root_children[0] {
            Child::Leaf(i) => {
                let leaf = &result.arena.leaves[*i as usize];
                assert_eq!(
                    table.get_string(leaf.key.normal).unwrap_or_default(),
                    "war_propaganda_decision_cost?150"
                );
            }
            other => panic!("expected a single key=value Leaf, got {:?}", other),
        }
    }

    #[test]
    fn caret_selector_key_is_one_keyed_clause() {
        // The `^` variant was already a key char; lock it in as a sibling case.
        let table = StringTable::new();
        let result =
            parse_string("war_propaganda_decision_cost^foo = { value = 150 }", &table).unwrap();
        assert_eq!(result.root_children.len(), 1, "{:?}", result.root_children);
        assert_eq!(
            keyed_clause_key(&result, &table, 0),
            "war_propaganda_decision_cost^foo"
        );
    }

    #[test]
    fn question_equal_operator_still_parses() {
        // `?=` is the QuestionEqual operator. Folding `?` into the key char set
        // must NOT break it: `key ?= value` and the no-space `key?= value` both
        // stay a single key=value leaf whose op is QuestionEqual, not a key with
        // a trailing `?`.
        for src in ["foo ?= bar", "foo?= bar"] {
            let table = StringTable::new();
            let result = parse_string(src, &table).unwrap();
            assert_eq!(
                result.root_children.len(),
                1,
                "`{}` must be one statement: {:?}",
                src,
                result.root_children
            );
            match &result.root_children[0] {
                Child::Leaf(i) => {
                    let leaf = &result.arena.leaves[*i as usize];
                    assert_eq!(
                        table.get_string(leaf.key.normal).unwrap_or_default(),
                        "foo",
                        "`{}` key must be `foo`, not carry the `?`",
                        src
                    );
                    assert_eq!(
                        leaf.op,
                        Operator::QuestionEqual,
                        "`{}` must keep the `?=` operator",
                        src
                    );
                }
                other => panic!("`{}`: expected a Leaf, got {:?}", src, other),
            }
        }
    }
}
