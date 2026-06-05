use cwtools_string_table::string_table::StringTokens;

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("{0}:{1}:{2}: {3}")]
    Pos(String, u32, u16, String),
    #[error("{0}")]
    General(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operator {
    Equals,
    GreaterThan,
    LessThan,
    GreaterThanOrEqual,
    LessThanOrEqual,
    NotEqual,
    EqualEqual,
    QuestionEqual,
}

impl Operator {
    pub fn as_str(&self) -> &'static str {
        match self {
            Operator::Equals => "=",
            Operator::GreaterThan => ">",
            Operator::LessThan => "<",
            Operator::GreaterThanOrEqual => ">=",
            Operator::LessThanOrEqual => "<=",
            Operator::NotEqual => "!=",
            Operator::EqualEqual => "==",
            Operator::QuestionEqual => "?=",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SourcePos {
    pub line: u32,
    pub col: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceRange {
    pub start: SourcePos,
    pub end: SourcePos,
}

// Arena indices
pub type NodeIdx = u32;
pub type LeafIdx = u32;
pub type LeafValueIdx = u32;
pub type ValueClauseIdx = u32;
pub type CommentIdx = u32;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    String(StringTokens),
    QString(StringTokens),
    Float(f64),
    Int(i64),
    Bool(bool),
    Clause(Vec<Child>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Child {
    Node(NodeIdx),
    Leaf(LeafIdx),
    LeafValue(LeafValueIdx),
    ValueClause(ValueClauseIdx),
    Comment(CommentIdx),
}

pub struct Leaf {
    pub key: StringTokens,
    pub value: Value,
    pub op: Operator,
    pub pos: SourceRange,
}

pub struct Node {
    pub key: StringTokens,
    pub children: Vec<Child>,
    pub pos: SourceRange,
    pub key_prefix: Option<StringTokens>,
    pub value_prefix: Option<StringTokens>,
}

pub struct LeafValue {
    pub value: Value,
    pub pos: SourceRange,
}

pub struct ValueClause {
    pub keys: Vec<StringTokens>,
    pub children: Vec<Child>,
    pub pos: SourceRange,
}

pub struct Comment {
    pub text: String,
    pub pos: SourceRange,
}

pub struct Arena {
    pub nodes: Vec<Node>,
    pub leaves: Vec<Leaf>,
    pub leaf_values: Vec<LeafValue>,
    pub value_clauses: Vec<ValueClause>,
    pub comments: Vec<Comment>,
}

impl Default for Arena {
    fn default() -> Self {
        Self::new()
    }
}

impl Arena {
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            leaves: Vec::new(),
            leaf_values: Vec::new(),
            value_clauses: Vec::new(),
            comments: Vec::new(),
        }
    }

    pub fn push_node(&mut self, node: Node) -> NodeIdx {
        let idx = self.nodes.len() as u32;
        self.nodes.push(node);
        idx
    }

    pub fn push_leaf(&mut self, leaf: Leaf) -> LeafIdx {
        let idx = self.leaves.len() as u32;
        self.leaves.push(leaf);
        idx
    }

    pub fn push_leaf_value(&mut self, lv: LeafValue) -> LeafValueIdx {
        let idx = self.leaf_values.len() as u32;
        self.leaf_values.push(lv);
        idx
    }

    pub fn push_value_clause(&mut self, vc: ValueClause) -> ValueClauseIdx {
        let idx = self.value_clauses.len() as u32;
        self.value_clauses.push(vc);
        idx
    }

    pub fn push_comment(&mut self, comment: Comment) -> CommentIdx {
        let idx = self.comments.len() as u32;
        self.comments.push(comment);
        idx
    }
}

pub struct ParsedFile {
    pub arena: Arena,
    pub root_children: Vec<Child>,
    pub errors: Vec<ParseError>,
}
