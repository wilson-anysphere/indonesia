use rowan::Language;
use serde_repr::{Deserialize_repr, Serialize_repr};

/// Version of the on-disk syntax schema used by `nova-cache`.
///
/// Bump this whenever serialized syntax artifacts become incompatible with
/// previously persisted data (e.g. `SyntaxKind` numeric values change, new token
/// kinds are inserted, node kinds are renamed/reordered, etc.).
pub const SYNTAX_SCHEMA_VERSION: u32 = 4;

/// Unified syntax kind for both tokens and AST nodes.
///
/// This enum is intentionally "fat": having a stable set of kinds is a
/// prerequisite for typed AST wrappers and downstream semantic analysis.
///
/// # Stability / Persistence
///
/// `SyntaxKind` is persisted as a `u16` in Nova's caches (see ADR0002). The
/// numeric discriminant of every variant is therefore part of the cache schema.
///
/// **Invariant:** do not reorder, renumber, or delete existing variants.
/// Only append new variants immediately before [`SyntaxKind::__Last`].
///
/// If an incompatible change is ever required (reordering/removal), the cache
/// schema version must be bumped so persisted data is not interpreted with the
/// wrong discriminants.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize_repr,
    Deserialize_repr,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
#[archive(check_bytes)]
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
    SwitchExpression,
    SwitchBlock,
    SwitchGroup,
    SwitchRule,
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
    YieldStatement,
    ReturnStatement,
    ThrowStatement,
    BreakStatement,
    ContinueStatement,
    LocalTypeDeclarationStatement,
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

    // Patterns (Java 16+ instanceof; Java 21+ switch)
    Pattern,
    TypePattern,
    RecordPattern,
    Guard,
    CaseLabelElement,

    // --- Synthetic missing tokens (zero-width) ---
    MissingSemicolon,
    MissingRParen,
    MissingRBrace,
    MissingRBracket,
    MissingGreater,

    // --- JPMS module declarations (module-info.java) ---
    ModuleDeclaration,
    ModuleBody,
    RequiresDirective,
    ExportsDirective,
    OpensDirective,
    UsesDirective,
    ProvidesDirective,

    DefaultValue,

    /// Root node produced by [`crate::parse_expression`].
    ExpressionRoot,
    /// Wrapper node for a single entry inside a module body.
    ///
    /// Kept at the end of the enum to preserve stable `SyntaxKind` discriminants
    /// for existing persisted syntax artifacts.
    ModuleDirective,

    // --- Java 21+ nodes (grammar-complete surface area) ---
    //
    // NOTE: New variants must be appended immediately before `__Last`.
    // See the stability notes on `SyntaxKind` for details.

    // Compilation unit variants / modules (JPMS).
    ModuleCompilationUnit,
    ModuleDirectiveList,

    // Additional type parameters & bounds.
    TypeParameterBound,
    AdditionalBound,

    // Type compositions.
    IntersectionType,
    UnionType,

    // Additional declaration clauses.
    ThrowsClause,

    // Methods / constructors.
    MethodHeader,
    MethodBody,
    ConstructorBody,
    ExplicitConstructorInvocation,
    ReceiverParameter,
    VarargsParameter,

    // Lambdas.
    LambdaParameters,
    LambdaParameterList,
    LambdaParameter,
    LambdaBody,

    // Records.
    RecordHeader,
    RecordComponentList,
    RecordComponent,
    CompactConstructorDeclaration,

    // Annotation type elements and values.
    AnnotationElementDeclaration,
    AnnotationElementValuePairList,
    AnnotationElementValuePair,
    AnnotationElementValueArrayInitializer,

    // Variable declarators / initializers.
    VariableDeclaratorId,
    VariableInitializer,
    ArrayInitializerList,

    // Additional type grammar needed for Java 8+ type annotations and arrays.
    AnnotatedType,
    ArrayType,
    ClassOrInterfaceType,
    ClassType,
    InterfaceType,
    TypeVariable,
    Dims,
    Dim,
    AnnotatedDim,
    DimExprs,
    DimExpr,
    WildcardBounds,
    WildcardExtendsBound,
    WildcardSuperBound,
    InferredTypeArguments,

    // Statements.
    BasicForStatement,
    EnhancedForStatement,
    ForInit,
    ForCondition,
    ForUpdate,
    EnhancedForControl,
    LocalClassDeclaration,
    LocalClassDeclarationStatement,

    // Switch (statement + expression) and pattern matching.
    SwitchRuleBody,
    SwitchBlockStatementGroup,
    CaseLabel,
    DefaultLabel,
    CaseLabelElementList,

    // Expressions.
    InstanceofExpression,
    ClassLiteralExpression,
    MethodReferenceExpression,
    ConstructorReferenceExpression,
    ClassInstanceCreationExpression,
    AnonymousClassBody,
    ArrayCreationExpression,
    PostfixExpression,
    PrefixExpression,
    ExplicitGenericInvocationExpression,

    // Pattern details (record patterns, constants, etc).
    RecordPatternComponentList,
    RecordPatternComponent,
    ParenthesizedPattern,
    ConstantPattern,
    NullPattern,
    UnnamedPattern,
    // Synthetic roots used when parsing fragments (blocks/expressions/etc.).
    BlockFragment,
    StatementFragment,
    ExpressionFragment,
    ClassMemberFragment,
    // --- Java String Templates (preview) ---
    //
    // NOTE: `StringTemplateExpression` is kept first to preserve stable `SyntaxKind` discriminants
    // for caches written after it was introduced.
    StringTemplateExpression,
    StringTemplateStart,
    StringTemplateText,
    StringTemplateExprStart,
    StringTemplateEnd,
    StringTemplate,
    StringTemplateInterpolation,
    __Last,
}

// Compile-time sanity check: if this ever fails, we've likely added an
// unreasonable amount of kinds or accidentally broken the `__Last` sentinel.
const _: [(); 1] = [(); (SyntaxKind::__Last as u16 <= 4096) as usize];

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

    pub fn is_keyword(self) -> bool {
        // Keep this fast without having to enumerate every keyword.
        let raw = self as u16;
        (SyntaxKind::AbstractKw as u16..=SyntaxKind::WhileKw as u16).contains(&raw)
            || matches!(
                self,
                SyntaxKind::TrueKw | SyntaxKind::FalseKw | SyntaxKind::NullKw
            )
            || self.is_contextual_keyword()
    }

    pub fn is_modifier_keyword(self) -> bool {
        matches!(
            self,
            SyntaxKind::PublicKw
                | SyntaxKind::ProtectedKw
                | SyntaxKind::PrivateKw
                | SyntaxKind::AbstractKw
                | SyntaxKind::StaticKw
                | SyntaxKind::FinalKw
                | SyntaxKind::NativeKw
                | SyntaxKind::SynchronizedKw
                | SyntaxKind::TransientKw
                | SyntaxKind::VolatileKw
                | SyntaxKind::StrictfpKw
                | SyntaxKind::DefaultKw
                | SyntaxKind::SealedKw
                | SyntaxKind::NonSealedKw
        )
    }

    pub fn is_literal(self) -> bool {
        matches!(
            self,
            SyntaxKind::Number
                | SyntaxKind::IntLiteral
                | SyntaxKind::LongLiteral
                | SyntaxKind::FloatLiteral
                | SyntaxKind::DoubleLiteral
                | SyntaxKind::CharLiteral
                | SyntaxKind::StringLiteral
                | SyntaxKind::TextBlock
                | SyntaxKind::TrueKw
                | SyntaxKind::FalseKw
                | SyntaxKind::NullKw
        )
    }

    pub fn is_operator(self) -> bool {
        matches!(
            self,
            SyntaxKind::Plus
                | SyntaxKind::Minus
                | SyntaxKind::Star
                | SyntaxKind::Slash
                | SyntaxKind::Percent
                | SyntaxKind::Tilde
                | SyntaxKind::Bang
                | SyntaxKind::Eq
                | SyntaxKind::EqEq
                | SyntaxKind::BangEq
                | SyntaxKind::Less
                | SyntaxKind::LessEq
                | SyntaxKind::Greater
                | SyntaxKind::GreaterEq
                | SyntaxKind::Amp
                | SyntaxKind::AmpAmp
                | SyntaxKind::AmpEq
                | SyntaxKind::Pipe
                | SyntaxKind::PipePipe
                | SyntaxKind::PipeEq
                | SyntaxKind::Caret
                | SyntaxKind::CaretEq
                | SyntaxKind::PlusPlus
                | SyntaxKind::MinusMinus
                | SyntaxKind::PlusEq
                | SyntaxKind::MinusEq
                | SyntaxKind::StarEq
                | SyntaxKind::SlashEq
                | SyntaxKind::PercentEq
                | SyntaxKind::LeftShift
                | SyntaxKind::RightShift
                | SyntaxKind::UnsignedRightShift
                | SyntaxKind::LeftShiftEq
                | SyntaxKind::RightShiftEq
                | SyntaxKind::UnsignedRightShiftEq
                | SyntaxKind::Question
                | SyntaxKind::Colon
                | SyntaxKind::Arrow
                | SyntaxKind::InstanceofKw
        )
    }

    pub fn is_separator(self) -> bool {
        matches!(
            self,
            SyntaxKind::LParen
                | SyntaxKind::RParen
                | SyntaxKind::LBrace
                | SyntaxKind::RBrace
                | SyntaxKind::LBracket
                | SyntaxKind::RBracket
                | SyntaxKind::Semicolon
                | SyntaxKind::Comma
                | SyntaxKind::Dot
                | SyntaxKind::Ellipsis
                | SyntaxKind::At
                | SyntaxKind::DoubleColon
        )
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
            "non-sealed" => SyntaxKind::NonSealedKw,
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
