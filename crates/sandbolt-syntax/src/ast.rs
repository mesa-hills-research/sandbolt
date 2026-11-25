use crate::Span;

macro_rules! id {
    ($name:ident) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub struct $name(pub(crate) u32);
        impl $name {
            pub const fn index(self) -> usize {
                self.0 as usize
            }
        }
    };
}

id!(ExprId);
id!(StmtId);
id!(PatternId);

#[derive(Debug, Clone)]
pub struct Node<T> {
    pub span: Span,
    pub kind: T,
}

#[derive(Debug, Clone, Default)]
pub struct Arena {
    pub(crate) expressions: Vec<Node<Expr>>,
    pub(crate) statements: Vec<Node<Stmt>>,
    pub(crate) patterns: Vec<Node<Pattern>>,
}

impl Arena {
    pub fn expr(&self, id: ExprId) -> &Node<Expr> {
        &self.expressions[id.index()]
    }
    pub fn stmt(&self, id: StmtId) -> &Node<Stmt> {
        &self.statements[id.index()]
    }
    pub fn pattern(&self, id: PatternId) -> &Node<Pattern> {
        &self.patterns[id.index()]
    }
    pub fn expressions(&self) -> &[Node<Expr>] {
        &self.expressions
    }
    pub fn statements(&self) -> &[Node<Stmt>] {
        &self.statements
    }
    pub fn patterns(&self) -> &[Node<Pattern>] {
        &self.patterns
    }
}

#[derive(Debug, Clone, Default)]
pub struct ParsedProgram {
    pub strict: bool,
    pub span: Span,
    pub body: Vec<StmtId>,
    pub arena: Arena,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum JsStrLit {
    Latin1(Vec<u8>),
    Utf16(Vec<u16>),
}

impl JsStrLit {
    pub fn from_units(units: Vec<u16>) -> Self {
        if units.iter().all(|&u| u <= 255) {
            Self::Latin1(units.into_iter().map(|u| u as u8).collect())
        } else {
            Self::Utf16(units)
        }
    }

    pub fn units(&self) -> impl Iterator<Item = u16> + '_ {
        let (latin1, utf16) = match self {
            Self::Latin1(bytes) => (bytes.as_slice(), &[][..]),
            Self::Utf16(units) => (&[][..], units.as_slice()),
        };
        latin1
            .iter()
            .copied()
            .map(u16::from)
            .chain(utf16.iter().copied())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NumericKind {
    Decimal,
    Binary,
    Octal,
    Hex,
    LegacyOctal,
    LegacyDecimal,
    BigInt { radix: u8 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NumericLiteral {
    pub raw: String,
    pub kind: NumericKind,
}

impl NumericLiteral {
    /// Convert a Number literal to binary64. BigInt literals retain their exact digits.
    pub fn parse_numeric_value(&self) -> Option<f64> {
        if matches!(self.kind, NumericKind::BigInt { .. }) {
            return None;
        }
        let (value, _) = sandbolt_dtoa::atod(
            &self.raw,
            0,
            sandbolt_dtoa::AtodFlags {
                accept_bin_oct: true,
                accept_legacy_octal: self.kind == NumericKind::LegacyOctal,
                accept_underscores: true,
                ..sandbolt_dtoa::AtodFlags::default()
            },
        );
        Some(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StringLiteral {
    pub value: JsStrLit,
    pub escaped: bool,
    pub legacy_escape: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegExpLiteral {
    pub pattern: JsStrLit,
    pub flags: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateElement {
    pub span: Span,
    pub raw: JsStrLit,
    pub cooked: Option<JsStrLit>,
}

#[derive(Debug, Clone)]
pub struct Template {
    pub elements: Vec<TemplateElement>,
    pub substitutions: Vec<ExprId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ident {
    pub span: Span,
    pub name: String,
    pub escaped: bool,
}

#[derive(Debug, Clone)]
pub enum Name {
    Ident(Ident),
    Private(Ident),
    String(StringLiteral),
    Number(NumericLiteral),
    Computed(ExprId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Plus,
    Minus,
    Not,
    BitNot,
    Delete,
    Void,
    Typeof,
    Await,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateOp {
    Inc,
    Dec,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Pow,
    Shl,
    Shr,
    UShr,
    BitAnd,
    BitXor,
    BitOr,
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
    StrictEq,
    StrictNe,
    In,
    Instanceof,
    And,
    Or,
    Nullish,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssignOp {
    Assign,
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Pow,
    Shl,
    Shr,
    UShr,
    BitAnd,
    BitXor,
    BitOr,
    And,
    Or,
    Nullish,
}

#[derive(Debug, Clone, Copy)]
pub enum Item {
    Value(ExprId),
    Spread(ExprId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodKind {
    Method,
    Getter,
    Setter,
    Constructor,
}

#[derive(Debug, Clone)]
pub enum Property {
    Value {
        key: Name,
        value: ExprId,
    },
    Shorthand(Ident),
    CoverInitialized {
        key: Ident,
        value: ExprId,
    },
    Method {
        key: Name,
        kind: MethodKind,
        function: Function,
    },
    Spread(ExprId),
}

#[derive(Debug, Clone)]
pub enum Expr {
    Identifier(Ident),
    PrivateIdentifier(Ident),
    This,
    Super,
    Null,
    Boolean(bool),
    Number(NumericLiteral),
    String(StringLiteral),
    RegExp(RegExpLiteral),
    Array {
        items: Vec<Option<Item>>,
        trailing_comma: bool,
    },
    Object {
        properties: Vec<Property>,
        trailing_comma: bool,
    },
    Function(Function),
    Class(Class),
    Template(Template),
    TaggedTemplate {
        tag: ExprId,
        template: Template,
    },
    Parenthesized(ExprId),
    Sequence(Vec<ExprId>),
    Unary {
        op: UnaryOp,
        argument: ExprId,
    },
    Update {
        op: UpdateOp,
        argument: ExprId,
        prefix: bool,
    },
    Binary {
        op: BinaryOp,
        left: ExprId,
        right: ExprId,
    },
    Conditional {
        test: ExprId,
        consequent: ExprId,
        alternate: ExprId,
    },
    Assignment {
        op: AssignOp,
        target: PatternId,
        value: ExprId,
    },
    Member {
        object: ExprId,
        key: Name,
        optional: bool,
    },
    Call {
        callee: ExprId,
        arguments: Vec<Item>,
        optional: bool,
    },
    Chain(ExprId),
    New {
        callee: ExprId,
        arguments: Vec<Item>,
    },
    NewTarget,
    ImportMeta,
    ImportCall {
        specifier: ExprId,
        options: Option<ExprId>,
    },
    Yield {
        argument: Option<ExprId>,
        delegate: bool,
    },
}

#[derive(Debug, Clone)]
pub enum Pattern {
    Identifier(Ident),
    /// The original expression retains the complete member or call reference.
    Reference(ExprId),
    Object {
        properties: Vec<PatternProperty>,
        rest: Option<PatternId>,
    },
    Array {
        items: Vec<Option<PatternId>>,
        rest: Option<PatternId>,
    },
    Default {
        target: PatternId,
        value: ExprId,
    },
    Rest(PatternId),
}

#[derive(Debug, Clone)]
pub struct PatternProperty {
    pub key: Name,
    pub target: PatternId,
}

#[derive(Debug, Clone)]
pub enum FunctionBody {
    Block(Vec<StmtId>),
    Expression(ExprId),
}

#[derive(Debug, Clone)]
pub struct Function {
    pub span: Span,
    pub name: Option<Ident>,
    pub params: Vec<PatternId>,
    pub body: FunctionBody,
    pub is_async: bool,
    pub is_generator: bool,
    pub is_arrow: bool,
    pub strict: bool,
    pub use_strict: bool,
}

#[derive(Debug, Clone)]
pub struct Class {
    pub name: Option<Ident>,
    pub extends: Option<ExprId>,
    pub elements: Vec<ClassElement>,
}

#[derive(Debug, Clone)]
pub enum ClassElement {
    Field {
        key: Name,
        initializer: Option<ExprId>,
        is_static: bool,
    },
    Method {
        key: Name,
        function: Function,
        kind: MethodKind,
        is_static: bool,
    },
    StaticBlock(Vec<StmtId>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclKind {
    Var,
    Let,
    Const,
    Using,
    AwaitUsing,
}

#[derive(Debug, Clone)]
pub struct Declarator {
    pub binding: PatternId,
    pub initializer: Option<ExprId>,
}

#[derive(Debug, Clone)]
pub struct Declaration {
    pub kind: DeclKind,
    pub entries: Vec<Declarator>,
}

#[derive(Debug, Clone)]
pub enum ForInit {
    Expression(ExprId),
    Declaration(Declaration),
}
#[derive(Debug, Clone)]
pub enum ForTarget {
    Pattern(PatternId),
    Declaration(Declaration),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForKind {
    In,
    Of,
    AwaitOf,
}

#[derive(Debug, Clone)]
pub struct SwitchCase {
    pub test: Option<ExprId>,
    pub body: Vec<StmtId>,
}

#[derive(Debug, Clone)]
pub struct Catch {
    pub binding: Option<PatternId>,
    pub body: StmtId,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ModuleName {
    Identifier(String),
    String(JsStrLit),
}

#[derive(Debug, Clone)]
pub struct ModuleRequest {
    pub specifier: JsStrLit,
    pub attributes: Vec<ImportAttribute>,
}

#[derive(Debug, Clone)]
pub struct ImportAttribute {
    pub key: ModuleName,
    pub value: JsStrLit,
}

#[derive(Debug, Clone)]
pub enum ImportBinding {
    Default(Ident),
    Namespace(Ident),
    Named { imported: ModuleName, local: Ident },
}

#[derive(Debug, Clone)]
pub struct ImportDeclaration {
    pub bindings: Vec<ImportBinding>,
    pub request: ModuleRequest,
}

#[derive(Debug, Clone)]
pub struct ExportSpecifier {
    pub local: ModuleName,
    pub exported: ModuleName,
}

#[derive(Debug, Clone)]
pub enum ExportDeclaration {
    Declaration(StmtId),
    DefaultDeclaration(StmtId),
    DefaultExpression(ExprId),
    Named {
        specifiers: Vec<ExportSpecifier>,
        request: Option<ModuleRequest>,
    },
    All {
        alias: Option<ModuleName>,
        request: ModuleRequest,
    },
}

#[derive(Debug, Clone)]
pub enum Stmt {
    Empty,
    Debugger,
    Expression(ExprId),
    Block(Vec<StmtId>),
    Declaration(Declaration),
    Function(Function),
    Class(Class),
    If {
        test: ExprId,
        consequent: StmtId,
        alternate: Option<StmtId>,
    },
    While {
        test: ExprId,
        body: StmtId,
    },
    DoWhile {
        body: StmtId,
        test: ExprId,
    },
    For {
        init: Option<ForInit>,
        test: Option<ExprId>,
        update: Option<ExprId>,
        body: StmtId,
    },
    ForEach {
        kind: ForKind,
        target: ForTarget,
        iterable: ExprId,
        body: StmtId,
    },
    Switch {
        discriminant: ExprId,
        cases: Vec<SwitchCase>,
    },
    Try {
        body: StmtId,
        catch: Option<Catch>,
        finally: Option<StmtId>,
    },
    With {
        object: ExprId,
        body: StmtId,
    },
    Label {
        name: Ident,
        body: StmtId,
    },
    Break(Option<Ident>),
    Continue(Option<Ident>),
    Return(Option<ExprId>),
    Throw(ExprId),
    Import(ImportDeclaration),
    Export(ExportDeclaration),
}
