//! Syntax tree for Roc.
//!
//! The tree is deliberately source-oriented: every node carries a byte range,
//! names carry their own ranges, and doc comments are attached to declarations
//! during parsing.  It is a lossless-enough structural view for an editor: the
//! original token stream lives alongside it in [`ParsedFile`], so exact text is
//! always available even when the parser recovers from an error.
//!
//! Methods are associated items inside a nominal type's `.{ ... }` block; they
//! appear as normal [`TypeDecl::members`] items, which keeps hover, completion,
//! and symbols uniform.

use crate::text::TextRange;

use super::token::Token;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Name {
    pub text: String,
    pub range: TextRange,
}

impl Name {
    pub fn new(text: impl Into<String>, range: TextRange) -> Self {
        Self {
            text: text.into(),
            range,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QualName {
    pub parts: Vec<Name>,
    pub range: TextRange,
}

impl QualName {
    pub fn as_string(&self) -> String {
        self.parts
            .iter()
            .map(|part| part.text.as_str())
            .collect::<Vec<_>>()
            .join(".")
    }

    pub fn first(&self) -> Option<&Name> {
        self.parts.first()
    }

    pub fn last(&self) -> Option<&Name> {
        self.parts.last()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Module {
    pub header: Option<ModuleHeader>,
    pub imports: Vec<Import>,
    pub items: Vec<Item>,
    pub range: TextRange,
}

impl Module {
    pub fn empty() -> Self {
        Self {
            header: None,
            imports: Vec::new(),
            items: Vec::new(),
            range: TextRange::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderKind {
    App,
    Module,
    Package,
    Platform,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModuleHeader {
    pub kind: HeaderKind,
    /// Platform name (the string after `platform`), or `None` for app/package.
    pub name: Option<Name>,
    /// The `[main!]` list for apps, or exposed modules for packages.
    pub provides: Vec<Name>,
    /// Dependencies from the header record (or platform `packages` section).
    pub dependencies: Vec<Dependency>,
    /// Platform `exposes` list.
    pub exposes: Vec<Name>,
    /// Platform `requires` type.
    pub requires: Option<TypeExpr>,
    /// Platform `provides` entries.
    pub provides_map: Vec<ProvidesEntry>,
    /// Platform `hosted` entries.
    pub hosted: Vec<HostedEntry>,
    /// Platform `targets` value, kept as a range for hover and links.
    pub targets: Option<TextRange>,
    pub range: TextRange,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DependencySource {
    Url(String),
    Path(String),
    RocVersion(String),
    /// A platform implemented by the compiler itself, such as `platform glue`.
    CompilerOwned(String),
}

impl DependencySource {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Url(value)
            | Self::Path(value)
            | Self::RocVersion(value)
            | Self::CompilerOwned(value) => value,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependency {
    pub shorthand: Name,
    pub is_platform: bool,
    pub source: DependencySource,
    pub range: TextRange,
    pub source_range: TextRange,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvidesEntry {
    pub symbol: Name,
    pub target: Name,
    pub range: TextRange,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostedEntry {
    pub symbol: Name,
    pub target: QualName,
    pub range: TextRange,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Import {
    pub path: ImportPath,
    pub alias: Option<Name>,
    /// The type annotation on `import "file" as name : Str`.
    pub ty: Option<TypeExpr>,
    pub exposing: Vec<Name>,
    pub range: TextRange,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ImportPath {
    /// A Roc module path: `Color`, `Src/Widget`, `json.Parser`, `Url.ParseErr`.
    /// Analysis decides whether the first lowercase segment is a package
    /// shorthand or a local file path.
    Roc {
        raw: String,
        segments: Vec<Name>,
        range: TextRange,
    },
    /// A compile-time file import: `import "config.json" as config : Str`.
    File { path: String, range: TextRange },
}

impl ImportPath {
    pub fn range(&self) -> TextRange {
        match self {
            Self::Roc { range, .. } | Self::File { range, .. } => *range,
        }
    }

    pub fn raw(&self) -> &str {
        match self {
            Self::Roc { raw, .. } => raw,
            Self::File { path, .. } => path,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Doc {
    pub text: String,
    pub range: TextRange,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub kind: ItemKind,
    pub range: TextRange,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ItemKind {
    Definition(Definition),
    /// A standalone top-level type annotation.  Parsers normally merge
    /// annotations into the definition that follows, but a recovered parse can
    /// leave one on its own; the editor still wants hover and symbols for it.
    Annotation(Annotation),
    TypeDecl(TypeDecl),
    WhereAlias(WhereAlias),
    Expect(Expect),
    Error,
}

impl Item {
    pub fn doc(&self) -> Option<&Doc> {
        match &self.kind {
            ItemKind::Definition(definition) => definition.doc.as_ref(),
            ItemKind::Annotation(annotation) => annotation.doc.as_ref(),
            ItemKind::TypeDecl(decl) => decl.doc.as_ref(),
            ItemKind::WhereAlias(alias) => alias.doc.as_ref(),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Annotation {
    pub name: Name,
    pub ty: TypeExpr,
    pub doc: Option<Doc>,
    pub range: TextRange,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Definition {
    pub pattern: Pattern,
    pub annotation: Option<TypeExpr>,
    pub value: Expr,
    pub is_var: bool,
    pub doc: Option<Doc>,
    pub range: TextRange,
    pub equals_range: TextRange,
}

impl Definition {
    pub fn simple_name(&self) -> Option<&Name> {
        match &self.pattern.kind {
            PatternKind::Name { name, .. } => Some(name),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeDeclKind {
    Alias,
    Nominal,
    Opaque,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TypeDecl {
    pub name: Name,
    pub kind: TypeDeclKind,
    pub params: Vec<Name>,
    pub backing: TypeExpr,
    pub members: Vec<Item>,
    pub doc: Option<Doc>,
    pub range: TextRange,
    pub operator_range: TextRange,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Expect {
    pub expr: Expr,
    pub doc: Option<Doc>,
    pub range: TextRange,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Expr {
    pub kind: ExprKind,
    pub range: TextRange,
}

impl Expr {
    pub fn new(kind: ExprKind, range: TextRange) -> Self {
        Self { kind, range }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExprKind {
    Number(String),
    String(Vec<StrPart>),
    MultilineString(Vec<StrPart>),
    Char(String),
    Tag {
        name: Name,
        args: Vec<Expr>,
    },
    List(Vec<Expr>),
    Record {
        fields: Vec<RecordField>,
        update: Option<Box<Expr>>,
    },
    Tuple(Vec<Expr>),
    Lambda {
        params: Vec<Pattern>,
        body: Box<Expr>,
    },
    Block {
        stmts: Vec<Stmt>,
        tail: Option<Box<Expr>>,
    },
    If {
        condition: Box<Expr>,
        then_branch: Box<Expr>,
        else_branch: Box<Expr>,
    },
    Match {
        subject: Box<Expr>,
        branches: Vec<MatchBranch>,
    },
    /// A bare name or dotted name chain: `foo`, `Str.concat`, `json.Parser`.
    Lookup {
        path: Vec<Name>,
    },
    Field {
        base: Box<Expr>,
        field: Name,
        optional: bool,
    },
    TupleIndex {
        base: Box<Expr>,
        index: u32,
        index_range: TextRange,
    },
    Call {
        callee: Box<Expr>,
        args: Vec<Expr>,
    },
    MethodCall {
        receiver: Box<Expr>,
        method: Name,
        args: Vec<Expr>,
    },
    Binary {
        op: String,
        op_range: TextRange,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    Unary {
        op: String,
        op_range: TextRange,
        expr: Box<Expr>,
    },
    Paren(Box<Expr>),
    For {
        pattern: Pattern,
        iterable: Box<Expr>,
        body: Box<Expr>,
    },
    While {
        condition: Box<Expr>,
        body: Box<Expr>,
    },
    Return(Option<Box<Expr>>),
    Break,
    Continue,
    Crash(Box<Expr>),
    Db(Box<Expr>),
    /// A nominal-type constructor or projection: `Point.{ x: 1 }`,
    /// `Distance.(5)`, `Color.Red`, or `NominalTypeRecord.{ x }`.  The parser
    /// produces this where the distinction is syntactically visible; analysis
    /// resolves whether `type_name` refers to a nominal type.
    Nominal {
        type_name: QualName,
        value: Box<Expr>,
    },
    Error,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StrPart {
    pub kind: StrPartKind,
    pub range: TextRange,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StrPartKind {
    Text,
    Interpolation(Expr),
}

#[derive(Debug, Clone, PartialEq)]
pub struct RecordField {
    pub name: Name,
    /// `None` means field punning (`{ name }`).
    pub value: Option<Expr>,
    /// `?:` in a record type; in a record literal this is always false.
    pub optional: bool,
    /// `field : Type ?? default` in a nominal record type.
    pub default: Option<Expr>,
    pub range: TextRange,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MatchBranch {
    /// Alternatives separated by `|`.
    pub patterns: Vec<Pattern>,
    pub guard: Option<Expr>,
    pub body: Expr,
    pub range: TextRange,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Stmt {
    pub kind: StmtKind,
    pub range: TextRange,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StmtKind {
    Definition(Definition),
    TypeDecl(TypeDecl),
    Assignment { pattern: Pattern, value: Expr },
    Db(Expr),
    Expect(Expr),
    Return(Option<Expr>),
    Break,
    Continue,
    Crash(Expr),
    Expr(Expr),
}

#[derive(Debug, Clone, PartialEq)]
pub struct WhereAlias {
    /// The type variable the requirements apply to (`a` in `a.Showable`).
    pub ty_var: Name,
    /// The alias name (`Showable`).
    pub name: Name,
    pub requirements: Vec<WhereRequirement>,
    pub doc: Option<Doc>,
    pub range: TextRange,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Pattern {
    pub kind: PatternKind,
    pub range: TextRange,
}

impl Pattern {
    pub fn new(kind: PatternKind, range: TextRange) -> Self {
        Self { kind, range }
    }

    /// Visit every name bound by this pattern.
    pub fn bindings(&self, output: &mut Vec<(String, TextRange, bool)>) {
        match &self.kind {
            PatternKind::Name { name, is_var } => {
                output.push((name.text.clone(), name.range, *is_var))
            }
            PatternKind::Tag { args, .. } => {
                for arg in args {
                    arg.bindings(output);
                }
            }
            PatternKind::Record { fields, rest } => {
                for field in fields {
                    if let Some(pattern) = &field.pattern {
                        pattern.bindings(output);
                    } else {
                        output.push((field.name.text.clone(), field.name.range, false));
                    }
                }
                if let Some(Some(name)) = rest {
                    output.push((name.text.clone(), name.range, false));
                }
            }
            PatternKind::Tuple(elements) | PatternKind::Alternatives(elements) => {
                for element in elements {
                    element.bindings(output);
                }
            }
            PatternKind::List { elements, rest } => {
                for element in elements {
                    element.bindings(output);
                }
                if let Some(Some(name)) = rest {
                    output.push((name.text.clone(), name.range, false));
                }
            }
            PatternKind::As { pattern, name } => {
                pattern.bindings(output);
                output.push((name.text.clone(), name.range, false));
            }
            PatternKind::Nominal { pattern, .. } => pattern.bindings(output),
            PatternKind::Wildcard
            | PatternKind::Number(_)
            | PatternKind::String(_)
            | PatternKind::Char(_)
            | PatternKind::Error => {}
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PatternKind {
    Name {
        name: Name,
        is_var: bool,
    },
    Wildcard,
    Number(String),
    String(Vec<StrPart>),
    Char(String),
    Tag {
        /// Present for qualified tags such as `Color.Red` or `Try.Ok(..)`.
        qualifier: Option<QualName>,
        name: Name,
        args: Vec<Pattern>,
    },
    Record {
        fields: Vec<PatternField>,
        /// `..`, `..rest`; `None` means closed.  `Some(None)` is a bare `..`.
        rest: Option<Option<Name>>,
    },
    Tuple(Vec<Pattern>),
    List {
        elements: Vec<Pattern>,
        rest: Option<Option<Name>>,
    },
    As {
        pattern: Box<Pattern>,
        name: Name,
    },
    Nominal {
        type_name: QualName,
        pattern: Box<Pattern>,
    },
    Alternatives(Vec<Pattern>),
    Error,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PatternField {
    pub name: Name,
    /// `{ name: pattern }`; `None` means shorthand `{ name }`.
    pub pattern: Option<Pattern>,
    pub range: TextRange,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TypeExpr {
    pub kind: TypeKind,
    pub range: TextRange,
}

impl TypeExpr {
    pub fn new(kind: TypeKind, range: TextRange) -> Self {
        Self { kind, range }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum TypeKind {
    Named(QualName),
    Var(Name),
    Wildcard,
    Function {
        args: Vec<TypeExpr>,
        ret: Box<TypeExpr>,
        effectful: bool,
    },
    Tuple(Vec<TypeExpr>),
    Record {
        fields: Vec<TypeField>,
        open: Option<Option<Name>>,
    },
    TagUnion {
        tags: Vec<TypeTag>,
        open: Option<Option<Name>>,
    },
    Apply {
        name: QualName,
        args: Vec<TypeExpr>,
    },
    Where {
        base: Box<TypeExpr>,
        requirements: Vec<WhereRequirement>,
    },
    Error,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TypeField {
    pub name: Name,
    pub ty: TypeExpr,
    pub optional: bool,
    pub default: Option<Expr>,
    pub range: TextRange,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TypeTag {
    pub name: Name,
    pub payloads: Vec<TypeExpr>,
    pub range: TextRange,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WhereRequirement {
    pub ty_var: Name,
    pub method: Name,
    pub ty: TypeExpr,
    pub range: TextRange,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedFile {
    pub module: Module,
    pub tokens: Vec<Token>,
    pub diagnostics: Vec<SyntaxDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxDiagnostic {
    pub range: TextRange,
    pub message: String,
    pub code: &'static str,
}
