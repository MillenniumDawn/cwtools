use crate::cache_format::*;
use cwtools_parser::ast::{
    Arena, Child, Comment, Leaf, LeafValue, Operator, SourcePos, SourceRange, Value, ValueClause,
};
use cwtools_string_table::string_table::{StringResolver, StringTable, StringTokens};

/// Convert an arena AST (with StringTable IDs) into a self-contained CachedFile.
pub fn arena_to_cached(
    arena: &Arena,
    root_children: &[Child],
    string_table: &StringTable,
) -> CachedFile {
    // Acquire the read lock once for the whole conversion rather than per token.
    string_table.with_read(|table| CachedFile {
        root_children: children_to_cached(root_children),
        leaves: arena
            .leaves
            .iter()
            .map(|l| leaf_to_cached(l, &table))
            .collect(),
        leaf_values: arena
            .leaf_values
            .iter()
            .map(|lv| leaf_value_to_cached(lv, &table))
            .collect(),
        value_clauses: arena
            .value_clauses
            .iter()
            .map(|vc| value_clause_to_cached(vc, &table))
            .collect(),
        comments: arena.comments.iter().map(comment_to_cached).collect(),
    })
}

/// Convert a CachedFile back into an arena AST, re-interning strings.
///
/// On a fresh `StringTable` every string is a miss, so interning each one
/// individually would take the write lock per string. Instead this collects
/// every string in the exact order `intern` would have been called, interns the
/// whole batch under a single write lock via
/// [`StringTable::intern_batch`](cwtools_string_table::string_table::StringTable::intern_batch),
/// then re-walks the nodes consuming the resulting tokens in the same order.
/// The token assignment is identical to per-string interning.
pub fn cached_to_arena(cached: &CachedFile, string_table: &StringTable) -> (Arena, Vec<Child>) {
    // Pass 1: collect every string slice in the same order `intern` is reached
    // when building leaves, then leaf_values, then value_clauses.
    let mut to_intern: Vec<&str> = Vec::new();
    for l in &cached.leaves {
        to_intern.push(&l.key);
        collect_value_strings(&l.value, &mut to_intern);
    }
    for lv in &cached.leaf_values {
        collect_value_strings(&lv.value, &mut to_intern);
    }
    for vc in &cached.value_clauses {
        for k in &vc.keys {
            to_intern.push(k);
        }
    }

    // Batch-intern under one write lock; tokens come back in collection order.
    let tokens = string_table.intern_batch(to_intern.iter().copied());
    let mut tokens = tokens.into_iter();

    // Pass 2: rebuild the arena, drawing tokens in the identical order.
    let mut arena = Arena::new();
    for l in &cached.leaves {
        let idx = arena.push_leaf(cached_leaf_to_leaf(l, &mut tokens));
        assert_eq!(idx as usize, arena.leaves.len() - 1);
    }
    for lv in &cached.leaf_values {
        let idx = arena.push_leaf_value(cached_leaf_value_to_leaf_value(lv, &mut tokens));
        assert_eq!(idx as usize, arena.leaf_values.len() - 1);
    }
    for vc in &cached.value_clauses {
        let idx = arena.push_value_clause(cached_value_clause_to_value_clause(vc, &mut tokens));
        assert_eq!(idx as usize, arena.value_clauses.len() - 1);
    }
    for c in &cached.comments {
        let idx = arena.push_comment(cached_comment_to_comment(c));
        assert_eq!(idx as usize, arena.comments.len() - 1);
    }
    debug_assert!(tokens.next().is_none(), "interned token count mismatch");

    let root = children_from_cached(&cached.root_children);
    (arena, root)
}

/// Push the strings a `CachedValue` contributes to interning, in field order.
/// `Clause` holds only child indices, so it contributes nothing here.
fn collect_value_strings<'a>(v: &'a CachedValue, out: &mut Vec<&'a str>) {
    match v {
        CachedValue::String(s) | CachedValue::QString(s) => out.push(s),
        CachedValue::Float(_)
        | CachedValue::Int(_)
        | CachedValue::Bool(_)
        | CachedValue::Clause(_) => {}
    }
}

// ---- helpers ----

fn string_token_to_str(token: &StringTokens, table: &StringResolver<'_>) -> String {
    table.get(token.normal).unwrap_or_default().to_string()
}

/// Draw the next pre-interned token from the batch. The batch was collected in
/// the exact order these are consumed, so each call yields the token for the
/// string that would have been re-interned at this point.
fn next_token(tokens: &mut impl Iterator<Item = StringTokens>) -> StringTokens {
    tokens.next().expect("interned token underrun")
}

fn range_to_cached(r: &SourceRange) -> (u32, u16, u32, u16) {
    (r.start.line, r.start.col, r.end.line, r.end.col)
}

fn cached_to_range(start_line: u32, start_col: u16, end_line: u32, end_col: u16) -> SourceRange {
    SourceRange {
        start: SourcePos {
            line: start_line,
            col: start_col,
        },
        end: SourcePos {
            line: end_line,
            col: end_col,
        },
    }
}

fn children_to_cached(children: &[Child]) -> Vec<CachedChild> {
    children
        .iter()
        .map(|c| match c {
            Child::Leaf(i) => CachedChild::Leaf(*i),
            Child::LeafValue(i) => CachedChild::LeafValue(*i),
            Child::ValueClause(i) => CachedChild::ValueClause(*i),
            Child::Comment(i) => CachedChild::Comment(*i),
        })
        .collect()
}

fn children_from_cached(children: &[CachedChild]) -> Vec<Child> {
    children
        .iter()
        .map(|c| match c {
            CachedChild::Leaf(i) => Child::Leaf(*i),
            CachedChild::LeafValue(i) => Child::LeafValue(*i),
            CachedChild::ValueClause(i) => Child::ValueClause(*i),
            CachedChild::Comment(i) => Child::Comment(*i),
        })
        .collect()
}

fn leaf_to_cached(l: &Leaf, table: &StringResolver<'_>) -> CachedLeaf {
    let (sl, sc, el, ec) = range_to_cached(&l.pos);
    CachedLeaf {
        key: string_token_to_str(&l.key, table),
        value: value_to_cached(&l.value, table),
        op: op_to_cached(&l.op),
        start_line: sl,
        start_col: sc,
        end_line: el,
        end_col: ec,
    }
}

fn cached_leaf_to_leaf(l: &CachedLeaf, tokens: &mut impl Iterator<Item = StringTokens>) -> Leaf {
    Leaf {
        key: next_token(tokens),
        value: cached_value_to_value(&l.value, tokens),
        op: cached_op_to_op(&l.op),
        pos: cached_to_range(l.start_line, l.start_col, l.end_line, l.end_col),
    }
}

fn leaf_value_to_cached(lv: &LeafValue, table: &StringResolver<'_>) -> CachedLeafValue {
    let (sl, sc, el, ec) = range_to_cached(&lv.pos);
    CachedLeafValue {
        value: value_to_cached(&lv.value, table),
        start_line: sl,
        start_col: sc,
        end_line: el,
        end_col: ec,
    }
}

fn cached_leaf_value_to_leaf_value(
    lv: &CachedLeafValue,
    tokens: &mut impl Iterator<Item = StringTokens>,
) -> LeafValue {
    LeafValue {
        value: cached_value_to_value(&lv.value, tokens),
        pos: cached_to_range(lv.start_line, lv.start_col, lv.end_line, lv.end_col),
    }
}

fn value_clause_to_cached(vc: &ValueClause, table: &StringResolver<'_>) -> CachedValueClause {
    let (sl, sc, el, ec) = range_to_cached(&vc.pos);
    CachedValueClause {
        keys: vc
            .keys
            .iter()
            .map(|k| string_token_to_str(k, table))
            .collect(),
        children: children_to_cached(&vc.children),
        start_line: sl,
        start_col: sc,
        end_line: el,
        end_col: ec,
    }
}

fn cached_value_clause_to_value_clause(
    vc: &CachedValueClause,
    tokens: &mut impl Iterator<Item = StringTokens>,
) -> ValueClause {
    ValueClause {
        keys: vc.keys.iter().map(|_| next_token(tokens)).collect(),
        children: children_from_cached(&vc.children),
        pos: cached_to_range(vc.start_line, vc.start_col, vc.end_line, vc.end_col),
    }
}

fn comment_to_cached(c: &Comment) -> CachedComment {
    let (sl, sc, el, ec) = range_to_cached(&c.pos);
    CachedComment {
        text: c.text.clone(),
        start_line: sl,
        start_col: sc,
        end_line: el,
        end_col: ec,
    }
}

fn cached_comment_to_comment(c: &CachedComment) -> Comment {
    Comment {
        text: c.text.clone(),
        pos: cached_to_range(c.start_line, c.start_col, c.end_line, c.end_col),
    }
}

fn value_to_cached(v: &Value, table: &StringResolver<'_>) -> CachedValue {
    match v {
        Value::String(t) => CachedValue::String(string_token_to_str(t, table)),
        Value::QString(t) => CachedValue::QString(string_token_to_str(t, table)),
        Value::Float(f) => CachedValue::Float(*f),
        Value::Int(i) => CachedValue::Int(*i),
        Value::Bool(b) => CachedValue::Bool(*b),
        Value::Clause(children) => CachedValue::Clause(children_to_cached(children)),
    }
}

fn cached_value_to_value(
    v: &CachedValue,
    tokens: &mut impl Iterator<Item = StringTokens>,
) -> Value {
    match v {
        CachedValue::String(_) => Value::String(next_token(tokens)),
        CachedValue::QString(_) => Value::QString(next_token(tokens)),
        CachedValue::Float(f) => Value::Float(*f),
        CachedValue::Int(i) => Value::Int(*i),
        CachedValue::Bool(b) => Value::Bool(*b),
        CachedValue::Clause(children) => Value::Clause(children_from_cached(children)),
    }
}

fn op_to_cached(op: &Operator) -> CachedOperator {
    match op {
        Operator::Equals => CachedOperator::Equals,
        Operator::GreaterThan => CachedOperator::GreaterThan,
        Operator::LessThan => CachedOperator::LessThan,
        Operator::GreaterThanOrEqual => CachedOperator::GreaterThanOrEqual,
        Operator::LessThanOrEqual => CachedOperator::LessThanOrEqual,
        Operator::NotEqual => CachedOperator::NotEqual,
        Operator::EqualEqual => CachedOperator::EqualEqual,
        Operator::QuestionEqual => CachedOperator::QuestionEqual,
    }
}

fn cached_op_to_op(op: &CachedOperator) -> Operator {
    match op {
        CachedOperator::Equals => Operator::Equals,
        CachedOperator::GreaterThan => Operator::GreaterThan,
        CachedOperator::LessThan => Operator::LessThan,
        CachedOperator::GreaterThanOrEqual => Operator::GreaterThanOrEqual,
        CachedOperator::LessThanOrEqual => Operator::LessThanOrEqual,
        CachedOperator::NotEqual => Operator::NotEqual,
        CachedOperator::EqualEqual => Operator::EqualEqual,
        CachedOperator::QuestionEqual => Operator::QuestionEqual,
    }
}
