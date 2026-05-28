use crate::cache_format::*;
use cwtools_parser::ast::{Arena, Child, Comment, Leaf, LeafValue, Node, Operator, SourcePos, SourceRange, Value, ValueClause};
use cwtools_string_table::string_table::StringTable;

/// Convert an arena AST (with StringTable IDs) into a self-contained CachedFile.
pub fn arena_to_cached(
    arena: &Arena,
    root_children: &[Child],
    string_table: &StringTable,
) -> CachedFile {
    CachedFile {
        root_children: children_to_cached(root_children),
        nodes: arena.nodes.iter().map(|n| node_to_cached(n, string_table)).collect(),
        leaves: arena.leaves.iter().map(|l| leaf_to_cached(l, string_table)).collect(),
        leaf_values: arena.leaf_values.iter().map(|lv| leaf_value_to_cached(lv, string_table)).collect(),
        value_clauses: arena.value_clauses.iter().map(|vc| value_clause_to_cached(vc, string_table)).collect(),
        comments: arena.comments.iter().map(|c| comment_to_cached(c)).collect(),
    }
}

/// Convert a CachedFile back into an arena AST, re-interning strings.
pub fn cached_to_arena(
    cached: &CachedFile,
    string_table: &StringTable,
) -> (Arena, Vec<Child>) {
    let mut arena = Arena::new();

    for n in &cached.nodes {
        let idx = arena.push_node(cached_node_to_node(n, string_table));
        assert_eq!(idx as usize, arena.nodes.len() - 1);
    }
    for l in &cached.leaves {
        let idx = arena.push_leaf(cached_leaf_to_leaf(l, string_table));
        assert_eq!(idx as usize, arena.leaves.len() - 1);
    }
    for lv in &cached.leaf_values {
        let idx = arena.push_leaf_value(cached_leaf_value_to_leaf_value(lv, string_table));
        assert_eq!(idx as usize, arena.leaf_values.len() - 1);
    }
    for vc in &cached.value_clauses {
        let idx = arena.push_value_clause(cached_value_clause_to_value_clause(vc, string_table));
        assert_eq!(idx as usize, arena.value_clauses.len() - 1);
    }
    for c in &cached.comments {
        let idx = arena.push_comment(cached_comment_to_comment(c));
        assert_eq!(idx as usize, arena.comments.len() - 1);
    }

    let root = children_from_cached(&cached.root_children, string_table, &mut arena);
    (arena, root)
}

// ---- helpers ----

fn string_token_to_str(token: &cwtools_string_table::string_table::StringTokens, table: &StringTable) -> String {
    table.get_string(token.normal).unwrap_or_default()
}

fn str_to_string_token(s: &str, table: &StringTable) -> cwtools_string_table::string_table::StringTokens {
    table.intern(s)
}

fn range_to_cached(r: &SourceRange) -> (u32, u16, u32, u16) {
    (r.start.line, r.start.col, r.end.line, r.end.col)
}

fn cached_to_range(start_line: u32, start_col: u16, end_line: u32, end_col: u16) -> SourceRange {
    SourceRange {
        start: SourcePos { line: start_line, col: start_col },
        end: SourcePos { line: end_line, col: end_col },
    }
}

fn children_to_cached(children: &[Child]) -> Vec<CachedChild> {
    children.iter().map(|c| match c {
        Child::Node(i) => CachedChild::Node(*i),
        Child::Leaf(i) => CachedChild::Leaf(*i),
        Child::LeafValue(i) => CachedChild::LeafValue(*i),
        Child::ValueClause(i) => CachedChild::ValueClause(*i),
        Child::Comment(i) => CachedChild::Comment(*i),
    }).collect()
}

fn children_from_cached(
    children: &[CachedChild],
    _table: &StringTable,
    _arena: &mut Arena,
) -> Vec<Child> {
    children.iter().map(|c| match c {
        CachedChild::Node(i) => Child::Node(*i),
        CachedChild::Leaf(i) => Child::Leaf(*i),
        CachedChild::LeafValue(i) => Child::LeafValue(*i),
        CachedChild::ValueClause(i) => Child::ValueClause(*i),
        CachedChild::Comment(i) => Child::Comment(*i),
    }).collect()
}

fn node_to_cached(n: &Node, table: &StringTable) -> CachedNode {
    let (sl, sc, el, ec) = range_to_cached(&n.pos);
    CachedNode {
        key: string_token_to_str(&n.key, table),
        key_prefix: n.key_prefix.as_ref().map(|t| string_token_to_str(t, table)),
        value_prefix: n.value_prefix.as_ref().map(|t| string_token_to_str(t, table)),
        children: children_to_cached(&n.children),
        start_line: sl,
        start_col: sc,
        end_line: el,
        end_col: ec,
    }
}

fn cached_node_to_node(n: &CachedNode, table: &StringTable) -> Node {
    Node {
        key: str_to_string_token(&n.key, table),
        key_prefix: n.key_prefix.as_ref().map(|s| str_to_string_token(s, table)),
        value_prefix: n.value_prefix.as_ref().map(|s| str_to_string_token(s, table)),
        children: children_from_cached(&n.children, table, &mut Arena::new()),
        pos: cached_to_range(n.start_line, n.start_col, n.end_line, n.end_col),
    }
}

fn leaf_to_cached(l: &Leaf, table: &StringTable) -> CachedLeaf {
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

fn cached_leaf_to_leaf(l: &CachedLeaf, table: &StringTable) -> Leaf {
    Leaf {
        key: str_to_string_token(&l.key, table),
        value: cached_value_to_value(&l.value, table),
        op: cached_op_to_op(&l.op),
        pos: cached_to_range(l.start_line, l.start_col, l.end_line, l.end_col),
    }
}

fn leaf_value_to_cached(lv: &LeafValue, table: &StringTable) -> CachedLeafValue {
    let (sl, sc, el, ec) = range_to_cached(&lv.pos);
    CachedLeafValue {
        value: value_to_cached(&lv.value, table),
        start_line: sl,
        start_col: sc,
        end_line: el,
        end_col: ec,
    }
}

fn cached_leaf_value_to_leaf_value(lv: &CachedLeafValue, table: &StringTable) -> LeafValue {
    LeafValue {
        value: cached_value_to_value(&lv.value, table),
        pos: cached_to_range(lv.start_line, lv.start_col, lv.end_line, lv.end_col),
    }
}

fn value_clause_to_cached(vc: &ValueClause, table: &StringTable) -> CachedValueClause {
    let (sl, sc, el, ec) = range_to_cached(&vc.pos);
    CachedValueClause {
        keys: vc.keys.iter().map(|k| string_token_to_str(k, table)).collect(),
        children: children_to_cached(&vc.children),
        start_line: sl,
        start_col: sc,
        end_line: el,
        end_col: ec,
    }
}

fn cached_value_clause_to_value_clause(vc: &CachedValueClause, table: &StringTable) -> ValueClause {
    ValueClause {
        keys: vc.keys.iter().map(|k| str_to_string_token(k, table)).collect(),
        children: children_from_cached(&vc.children, table, &mut Arena::new()),
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

fn value_to_cached(v: &Value, table: &StringTable) -> CachedValue {
    match v {
        Value::String(t) => CachedValue::String(string_token_to_str(t, table)),
        Value::QString(t) => CachedValue::QString(string_token_to_str(t, table)),
        Value::Float(f) => CachedValue::Float(*f),
        Value::Int(i) => CachedValue::Int(*i),
        Value::Bool(b) => CachedValue::Bool(*b),
        Value::Clause(children) => CachedValue::Clause(children_to_cached(children)),
    }
}

fn cached_value_to_value(v: &CachedValue, table: &StringTable) -> Value {
    match v {
        CachedValue::String(s) => Value::String(str_to_string_token(s, table)),
        CachedValue::QString(s) => Value::QString(str_to_string_token(s, table)),
        CachedValue::Float(f) => Value::Float(*f),
        CachedValue::Int(i) => Value::Int(*i),
        CachedValue::Bool(b) => Value::Bool(*b),
        CachedValue::Clause(children) => Value::Clause(children_from_cached(children, table, &mut Arena::new())),
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
