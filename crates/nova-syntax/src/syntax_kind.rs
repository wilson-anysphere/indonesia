use serde_repr::{Deserialize_repr, Serialize_repr};
use rowan::Language;

/// Unified syntax kind for both tokens and AST nodes.
///
/// This enum is intentionally "fat": having a stable set of kinds is a
/// prerequisite for typed AST wrappers and downstream semantic analysis.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize_repr, Deserialize_repr,
)]
#[repr(u16)]
pub enum SyntaxKind {
    // --- Trivia ---
    Whitespace,
    LineComment,
    BlockComment,
    DocComment,

    // --- Identifiers & literals ---
    Identifier,
    /// Legacy numeric literal used by the cache layer's token-level parser.
    Number,
    IntLiteral,
    LongLiteral,
    FloatLiteral,
    DoubleLiteral,
    CharLiteral,
    StringLiteral,
    TextBlock,
    /// Legacy catch-all punctuation token used by the cache layer.
    Punctuation,

    // --- Keywords (reserved) ---
    AbstractKw,
    AssertKw,
    BooleanKw,
    BreakKw,
    ByteKw,
    CaseKw,
    CatchKw,
    CharKw,
    ClassKw,
    ConstKw,
    ContinueKw,
    DefaultKw,
    DoKw,
    DoubleKw,
    ElseKw,
    EnumKw,
    ExtendsKw,
    FinalKw,
    FinallyKw,
    FloatKw,
    ForKw,
    GotoKw,
    IfKw,
    ImplementsKw,
    ImportKw,
    InstanceofKw,
    IntKw,
    InterfaceKw,
    LongKw,
    NativeKw,
    NewKw,
    PackageKw,
    PrivateKw,
    ProtectedKw,
    PublicKw,
    ReturnKw,
    ShortKw,
    StaticKw,
    StrictfpKw,
    SuperKw,
    SwitchKw,
    SynchronizedKw,
    ThisKw,
    ThrowKw,
    ThrowsKw,
    TransientKw,
    TryKw,
    VoidKw,
    VolatileKw,
    WhileKw,

    // Literal keywords.
    TrueKw,
    FalseKw,
    NullKw,

    // --- Contextual / restricted keywords ---
    VarKw,
    YieldKw,
    RecordKw,
    SealedKw,
    PermitsKw,
    NonSealedKw,
    WhenKw,
    ModuleKw,
    OpenKw,
    OpensKw,
    RequiresKw,
    TransitiveKw,
    ExportsKw,
    ToKw,
    UsesKw,
    ProvidesKw,
    WithKw,

    // --- Operators / punctuation ---
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Semicolon,
    Comma,
    Dot,
    Ellipsis,
    At,
    Question,
    Colon,
    DoubleColon,
    Arrow,

    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Tilde,
    Bang,

    Eq,
    EqEq,
    BangEq,

    Less,
    LessEq,
    Greater,
    GreaterEq,

    Amp,
    AmpAmp,
    AmpEq,
    Pipe,
    PipePipe,
    PipeEq,
    Caret,
    CaretEq,

    PlusPlus,
    MinusMinus,

    PlusEq,
    MinusEq,
    StarEq,
    SlashEq,
    PercentEq,

    LeftShift,
    RightShift,
    UnsignedRightShift,
    LeftShiftEq,
    RightShiftEq,
    UnsignedRightShiftEq,

    // --- Special ---
    Error,
    Eof,

    // --- Nodes ---
    CompilationUnit,
    PackageDeclaration,
    ImportDeclaration,
    Modifiers,
    Annotation,
    Name,

    ClassDeclaration,
    InterfaceDeclaration,
    EnumDeclaration,
    RecordDeclaration,
    AnnotationTypeDeclaration,
    ClassBody,
    InterfaceBody,
    EnumBody,
    RecordBody,
    AnnotationBody,
    EnumConstant,

    FieldDeclaration,
    MethodDeclaration,
    ConstructorDeclaration,
    InitializerBlock,
    EmptyDeclaration,
    ParameterList,
    Parameter,

    Block,
    LabeledStatement,
    IfStatement,
    SwitchStatement,
    SwitchBlock,
    SwitchLabel,
    ForStatement,
    ForHeader,
    WhileStatement,
    DoWhileStatement,
    SynchronizedStatement,
    TryStatement,
    ResourceSpecification,
    Resource,
    CatchClause,
    FinallyClause,
    AssertStatement,
    ReturnStatement,
    ThrowStatement,
    BreakStatement,
    ContinueStatement,
    LocalVariableDeclarationStatement,
    ExpressionStatement,
    EmptyStatement,

    VariableDeclaratorList,
    VariableDeclarator,

    Type,
    PrimitiveType,
    NamedType,
    TypeArguments,
    TypeArgument,
    WildcardType,

    ArgumentList,

    // Expressions
    LiteralExpression,
    NameExpression,
    ThisExpression,
    SuperExpression,
    ParenthesizedExpression,
    NewExpression,
    MethodCallExpression,
    FieldAccessExpression,
    ArrayAccessExpression,
    UnaryExpression,
    BinaryExpression,
    AssignmentExpression,
    ConditionalExpression,
    LambdaExpression,
    CastExpression,

    // --- Declarations / generics / annotation defaults ---
    ExtendsClause,
    ImplementsClause,
    PermitsClause,
    TypeParameters,
    TypeParameter,
    AnnotationElementDefault,
    AnnotationElementValue,
    ArrayInitializer,

    __Last,
}

impl SyntaxKind {
    pub fn is_trivia(self) -> bool {
        matches!(
            self,
            SyntaxKind::Whitespace
                | SyntaxKind::LineComment
                | SyntaxKind::BlockComment
                | SyntaxKind::DocComment
        )
    }

    pub fn is_contextual_keyword(self) -> bool {
        matches!(
            self,
            SyntaxKind::VarKw
                | SyntaxKind::YieldKw
                | SyntaxKind::RecordKw
                | SyntaxKind::SealedKw
                | SyntaxKind::PermitsKw
                | SyntaxKind::NonSealedKw
                | SyntaxKind::WhenKw
                | SyntaxKind::ModuleKw
                | SyntaxKind::OpenKw
                | SyntaxKind::OpensKw
                | SyntaxKind::RequiresKw
                | SyntaxKind::TransitiveKw
                | SyntaxKind::ExportsKw
                | SyntaxKind::ToKw
                | SyntaxKind::UsesKw
                | SyntaxKind::ProvidesKw
                | SyntaxKind::WithKw
        )
    }

    pub fn is_identifier_like(self) -> bool {
        self == SyntaxKind::Identifier || self.is_contextual_keyword()
    }

    pub fn from_keyword(text: &str) -> Option<SyntaxKind> {
        Some(match text {
            // Reserved keywords.
            "abstract" => SyntaxKind::AbstractKw,
            "assert" => SyntaxKind::AssertKw,
            "boolean" => SyntaxKind::BooleanKw,
            "break" => SyntaxKind::BreakKw,
            "byte" => SyntaxKind::ByteKw,
            "case" => SyntaxKind::CaseKw,
            "catch" => SyntaxKind::CatchKw,
            "char" => SyntaxKind::CharKw,
            "class" => SyntaxKind::ClassKw,
            "const" => SyntaxKind::ConstKw,
            "continue" => SyntaxKind::ContinueKw,
            "default" => SyntaxKind::DefaultKw,
            "do" => SyntaxKind::DoKw,
            "double" => SyntaxKind::DoubleKw,
            "else" => SyntaxKind::ElseKw,
            "enum" => SyntaxKind::EnumKw,
            "extends" => SyntaxKind::ExtendsKw,
            "final" => SyntaxKind::FinalKw,
            "finally" => SyntaxKind::FinallyKw,
            "float" => SyntaxKind::FloatKw,
            "for" => SyntaxKind::ForKw,
            "goto" => SyntaxKind::GotoKw,
            "if" => SyntaxKind::IfKw,
            "implements" => SyntaxKind::ImplementsKw,
            "import" => SyntaxKind::ImportKw,
            "instanceof" => SyntaxKind::InstanceofKw,
            "int" => SyntaxKind::IntKw,
            "interface" => SyntaxKind::InterfaceKw,
            "long" => SyntaxKind::LongKw,
            "native" => SyntaxKind::NativeKw,
            "new" => SyntaxKind::NewKw,
            "package" => SyntaxKind::PackageKw,
            "private" => SyntaxKind::PrivateKw,
            "protected" => SyntaxKind::ProtectedKw,
            "public" => SyntaxKind::PublicKw,
            "return" => SyntaxKind::ReturnKw,
            "short" => SyntaxKind::ShortKw,
            "static" => SyntaxKind::StaticKw,
            "strictfp" => SyntaxKind::StrictfpKw,
            "super" => SyntaxKind::SuperKw,
            "switch" => SyntaxKind::SwitchKw,
            "synchronized" => SyntaxKind::SynchronizedKw,
            "this" => SyntaxKind::ThisKw,
            "throw" => SyntaxKind::ThrowKw,
            "throws" => SyntaxKind::ThrowsKw,
            "transient" => SyntaxKind::TransientKw,
            "try" => SyntaxKind::TryKw,
            "void" => SyntaxKind::VoidKw,
            "volatile" => SyntaxKind::VolatileKw,
            "while" => SyntaxKind::WhileKw,

            // Literal keywords.
            "true" => SyntaxKind::TrueKw,
            "false" => SyntaxKind::FalseKw,
            "null" => SyntaxKind::NullKw,

            // Restricted keywords / contextual.
            "var" => SyntaxKind::VarKw,
            "yield" => SyntaxKind::YieldKw,
            "record" => SyntaxKind::RecordKw,
            "sealed" => SyntaxKind::SealedKw,
            "permits" => SyntaxKind::PermitsKw,
            "when" => SyntaxKind::WhenKw,
            "module" => SyntaxKind::ModuleKw,
            "open" => SyntaxKind::OpenKw,
            "opens" => SyntaxKind::OpensKw,
            "requires" => SyntaxKind::RequiresKw,
            "transitive" => SyntaxKind::TransitiveKw,
            "exports" => SyntaxKind::ExportsKw,
            "to" => SyntaxKind::ToKw,
            "uses" => SyntaxKind::UsesKw,
            "provides" => SyntaxKind::ProvidesKw,
            "with" => SyntaxKind::WithKw,

            _ => return None,
        })
    }
}

impl From<SyntaxKind> for rowan::SyntaxKind {
    fn from(value: SyntaxKind) -> Self {
        rowan::SyntaxKind(value as u16)
    }
}

/// Rowan language marker for Java.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum JavaLanguage {}

impl Language for JavaLanguage {
    type Kind = SyntaxKind;

    fn kind_from_raw(raw: rowan::SyntaxKind) -> SyntaxKind {
        if raw.0 < SyntaxKind::__Last as u16 {
            // SAFETY: We've verified the numeric value is within the enum range.
            unsafe { std::mem::transmute::<u16, SyntaxKind>(raw.0) }
        } else {
            SyntaxKind::Error
        }
    }

    fn kind_to_raw(kind: SyntaxKind) -> rowan::SyntaxKind {
        kind.into()
    }
}
