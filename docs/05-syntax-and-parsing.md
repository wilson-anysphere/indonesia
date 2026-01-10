# 05 - Syntax and Parsing

[← Back to Main Document](../AGENTS.md) | [Previous: Incremental Computation](04-incremental-computation.md)

## Overview

The syntax layer is the foundation of all language intelligence. Nova's parser must handle Java's complex grammar while providing excellent error recovery, incremental reparsing, and lossless representation.

---

## Design Goals

```
┌─────────────────────────────────────────────────────────────────┐
│                    PARSER DESIGN GOALS                           │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  1. ERROR RESILIENCE                                            │
│     • Always produce a syntax tree, even with errors            │
│     • Errors should not cascade unnecessarily                   │
│     • Recover quickly and continue parsing                      │
│     • Preserve maximum useful information                       │
│                                                                  │
│  2. FULL FIDELITY                                               │
│     • Preserve all source text (whitespace, comments)           │
│     • Round-trip: parse → unparse = original text               │
│     • Support accurate source mapping                           │
│                                                                  │
│  3. INCREMENTAL                                                  │
│     • Reparse only changed regions                              │
│     • Reuse unchanged subtrees                                  │
│     • Sub-millisecond updates for typical edits                 │
│                                                                  │
│  4. PERFORMANT                                                   │
│     • Parse megabyte files in milliseconds                      │
│     • Memory-efficient tree representation                      │
│     • Support lazy parsing where beneficial                     │
│                                                                  │
│  5. CORRECT                                                      │
│     • Full Java language support (through Java 21+)             │
│     • Match JLS grammar exactly for valid code                  │
│     • Handle preview features appropriately                     │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

---

## Syntax Tree Architecture

### Red-Green Tree Design

Nova uses a **red-green tree** architecture (pioneered by Roslyn):

```
┌─────────────────────────────────────────────────────────────────┐
│                    RED-GREEN TREE                                │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  GREEN TREE (Immutable, Position-Independent)                   │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │                                                         │    │
│  │  GreenNode {                                            │    │
│  │    kind: SyntaxKind,     // e.g., MethodDeclaration    │    │
│  │    text_len: u32,        // Total text length          │    │
│  │    children: Arc<[GreenChild]>,                        │    │
│  │  }                                                      │    │
│  │                                                         │    │
│  │  GreenChild = GreenNode | GreenToken                   │    │
│  │                                                         │    │
│  │  GreenToken {                                           │    │
│  │    kind: SyntaxKind,     // e.g., Identifier           │    │
│  │    text: Arc<str>,       // Actual text content        │    │
│  │  }                                                      │    │
│  │                                                         │    │
│  │  Properties:                                            │    │
│  │  • No position information → can be shared/cached      │    │
│  │  • Reference counted → structural sharing              │    │
│  │  • Immutable → thread-safe                             │    │
│  │                                                         │    │
│  └─────────────────────────────────────────────────────────┘    │
│                                                                  │
│  RED TREE (Mutable Wrapper, Position-Aware)                     │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │                                                         │    │
│  │  RedNode {                                              │    │
│  │    green: &GreenNode,    // Points to green node       │    │
│  │    parent: Option<&RedNode>,                           │    │
│  │    offset: u32,          // Absolute position          │    │
│  │  }                                                      │    │
│  │                                                         │    │
│  │  Properties:                                            │    │
│  │  • Created on-demand during traversal                  │    │
│  │  • Provides absolute positions                         │    │
│  │  • Enables parent navigation                           │    │
│  │  • Cheap to create (single allocation)                 │    │
│  │                                                         │    │
│  └─────────────────────────────────────────────────────────┘    │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### Why Red-Green?

```
┌─────────────────────────────────────────────────────────────────┐
│                    BENEFITS OF RED-GREEN                         │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  STRUCTURAL SHARING                                              │
│                                                                  │
│  Before edit:                   After editing method body:      │
│  ┌────────────┐                 ┌────────────┐                  │
│  │ ClassDecl  │                 │ ClassDecl' │                  │
│  └─────┬──────┘                 └─────┬──────┘                  │
│        │                              │                          │
│  ┌─────┴─────┐                  ┌─────┴─────┐                   │
│  │           │                  │           │                    │
│  ▼           ▼                  ▼           ▼                    │
│ Field     Method              Field      Method'                │
│  │          │                  │           │                     │
│  │          │                  │           │                     │
│  ▼          ▼                  │           ▼                     │
│ (data)    Body                 │         Body'                  │
│             │                  │           │                     │
│             │                  │           │  ONLY NEW NODES     │
│                                └───────────┘  ARE ALLOCATED      │
│                                                                  │
│  • Field subtree REUSED (same green node)                       │
│  • Only changed path re-allocated                               │
│  • Memory efficient for incremental edits                       │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

---

## Lexer Design

### Token Types

```rust
/// Comprehensive Java token kinds
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyntaxKind {
    // Trivia (preserved but not semantic)
    Whitespace,
    LineComment,      // // comment
    BlockComment,     // /* comment */
    DocComment,       // /** javadoc */
    
    // Keywords
    AbstractKw,
    AssertKw,
    BooleanKw,
    BreakKw,
    // ... all Java keywords
    
    // Contextual keywords (identifiers in some contexts)
    VarKw,           // var (Java 10+)
    YieldKw,         // yield (Java 13+)
    RecordKw,        // record (Java 16+)
    SealedKw,        // sealed (Java 17+)
    PermitsKw,       // permits (Java 17+)
    
    // Literals
    IntLiteral,
    LongLiteral,
    FloatLiteral,
    DoubleLiteral,
    CharLiteral,
    StringLiteral,
    TextBlock,       // """ multiline """ (Java 15+)
    
    // Operators
    Plus,            // +
    Minus,           // -
    Star,            // *
    Slash,           // /
    // ... all operators
    
    // Punctuation
    LeftParen,       // (
    RightParen,      // )
    LeftBrace,       // {
    RightBrace,      // }
    // ... all punctuation
    
    // Identifiers
    Identifier,
    
    // Error tokens
    Error,           // Lexer error
    
    // Composite nodes (not tokens, but share enum for simplicity)
    CompilationUnit,
    PackageDeclaration,
    ImportDeclaration,
    ClassDeclaration,
    // ... all node kinds
}
```

### Lexer Implementation

```rust
/// High-performance lexer
pub struct Lexer<'a> {
    input: &'a str,
    position: usize,
    
    /// Support for incremental lexing
    /// Tokens that span across edit boundary need re-lexing
    cached_tokens: Option<&'a [Token]>,
    edit_range: Option<TextRange>,
}

impl<'a> Lexer<'a> {
    /// Lex the next token
    pub fn next_token(&mut self) -> Token {
        self.skip_trivia();
        
        let start = self.position;
        let kind = self.scan_token();
        let end = self.position;
        
        Token {
            kind,
            range: TextRange::new(start, end),
        }
    }
    
    fn scan_token(&mut self) -> SyntaxKind {
        let c = self.peek();
        
        match c {
            // Fast path for common cases
            'a'..='z' | 'A'..='Z' | '_' | '$' => self.scan_identifier_or_keyword(),
            '0'..='9' => self.scan_number(),
            '"' => self.scan_string(),
            '\'' => self.scan_char(),
            '/' if self.peek_ahead(1) == '/' => self.scan_line_comment(),
            '/' if self.peek_ahead(1) == '*' => self.scan_block_comment(),
            
            // Operators and punctuation
            '+' => self.scan_plus(),  // +, ++, +=
            '-' => self.scan_minus(), // -, --, -=, ->
            // ... etc
            
            _ => {
                self.advance();
                SyntaxKind::Error
            }
        }
    }
}
```

---

## Parser Design

### Parser Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                    PARSER ARCHITECTURE                           │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  INPUT                                                          │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │  Source text: "public class Foo { void bar() {} }"     │    │
│  └─────────────────────────────────────────────────────────┘    │
│                              │                                   │
│                              ▼                                   │
│  LEXER                                                          │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │  Tokens: [public] [class] [Foo] [{] [void] [bar] ...   │    │
│  └─────────────────────────────────────────────────────────┘    │
│                              │                                   │
│                              ▼                                   │
│  PARSER (Recursive Descent with Pratt for expressions)         │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │  • Hand-written recursive descent                       │    │
│  │  • Pratt parsing for expressions (handles precedence)   │    │
│  │  • Error recovery at statement/declaration boundaries   │    │
│  │  • Builds green tree incrementally                      │    │
│  └─────────────────────────────────────────────────────────┘    │
│                              │                                   │
│                              ▼                                   │
│  OUTPUT                                                         │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │  GreenNode tree + error list                            │    │
│  └─────────────────────────────────────────────────────────┘    │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### Tree Builder

```rust
/// Builder that constructs green trees
pub struct TreeBuilder {
    /// Stack of in-progress nodes
    parents: Vec<(SyntaxKind, Vec<GreenChild>)>,
    
    /// Errors encountered during parsing
    errors: Vec<ParseError>,
}

impl TreeBuilder {
    /// Start a new node
    pub fn start_node(&mut self, kind: SyntaxKind) {
        self.parents.push((kind, Vec::new()));
    }
    
    /// Finish the current node
    pub fn finish_node(&mut self) {
        let (kind, children) = self.parents.pop().unwrap();
        let node = GreenNode::new(kind, children);
        
        if let Some((_, parent_children)) = self.parents.last_mut() {
            parent_children.push(GreenChild::Node(node));
        }
    }
    
    /// Add a token to current node
    pub fn token(&mut self, kind: SyntaxKind, text: &str) {
        let token = GreenToken::new(kind, text);
        if let Some((_, children)) = self.parents.last_mut() {
            children.push(GreenChild::Token(token));
        }
    }
    
    /// Record an error
    pub fn error(&mut self, message: &str, range: TextRange) {
        self.errors.push(ParseError { message: message.into(), range });
    }
}
```

### Error Recovery Strategies

```
┌─────────────────────────────────────────────────────────────────┐
│                    ERROR RECOVERY STRATEGIES                     │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  STRATEGY 1: SYNCHRONIZATION POINTS                             │
│  Skip tokens until reaching a "safe" point                      │
│                                                                  │
│  Input: "class Foo { int x = ; void bar() {} }"                 │
│                        ERROR ↑                                   │
│  Recovery: Skip until ';', continue parsing                     │
│  Result: x declaration marked as error, bar() parsed correctly  │
│                                                                  │
│  STRATEGY 2: INSERTION                                           │
│  Insert expected token and continue                             │
│                                                                  │
│  Input: "class Foo { void bar( {} }"                            │
│                           MISSING ')' ↑                          │
│  Recovery: Insert phantom ')', continue parsing                 │
│  Result: Method parsed with error, body intact                  │
│                                                                  │
│  STRATEGY 3: REPLACEMENT                                         │
│  Treat unexpected token as different token                      │
│                                                                  │
│  Input: "class Foo { in x; }"  // typo: 'in' instead of 'int'  │
│  Recovery: Treat 'in' as identifier, mark error                 │
│  Result: Field declaration parsed as best effort                │
│                                                                  │
│  STRATEGY 4: NESTED RECOVERY                                     │
│  Enter sub-expression recovery                                  │
│                                                                  │
│  Input: "int x = foo(bar, , baz);"                              │
│  Recovery: Insert missing expression, note error                │
│  Result: Method call with error marker for missing arg          │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### Recovery Implementation

```rust
impl Parser {
    /// Parse a class member declaration with recovery
    fn parse_class_member(&mut self) -> Option<GreenNode> {
        // Try to parse normally
        if let Some(member) = self.try_parse_member() {
            return Some(member);
        }
        
        // Recovery: skip to next member or closing brace
        self.builder.start_node(SyntaxKind::Error);
        self.recover_to(&[
            SyntaxKind::PublicKw,
            SyntaxKind::PrivateKw,
            SyntaxKind::ProtectedKw,
            SyntaxKind::StaticKw,
            SyntaxKind::ClassKw,
            SyntaxKind::InterfaceKw,
            SyntaxKind::RightBrace,
        ]);
        self.builder.finish_node();
        
        // Return None to signal error, but parsing continues
        None
    }
    
    /// Skip tokens until reaching one of the recovery set
    fn recover_to(&mut self, recovery_set: &[SyntaxKind]) {
        while !self.at_eof() {
            if recovery_set.contains(&self.current()) {
                break;
            }
            // Include skipped token in error node
            self.bump();
        }
    }
}
```

---

## Incremental Parsing

### Change Processing

```
┌─────────────────────────────────────────────────────────────────┐
│                    INCREMENTAL PARSING                           │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  SCENARIO: User types 'x' inside a method body                  │
│                                                                  │
│  Before:                                                        │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │  void foo() {                                           │    │
│  │    int y = 1;                                           │    │
│  │    |  ← cursor here                                     │    │
│  │  }                                                      │    │
│  └─────────────────────────────────────────────────────────┘    │
│                                                                  │
│  After typing 'x':                                              │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │  void foo() {                                           │    │
│  │    int y = 1;                                           │    │
│  │    x|  ← cursor here                                    │    │
│  │  }                                                      │    │
│  └─────────────────────────────────────────────────────────┘    │
│                                                                  │
│  INCREMENTAL STRATEGY:                                          │
│  1. Identify changed region: single character insert            │
│  2. Find containing node: method body block                     │
│  3. Reparse ONLY the block                                      │
│  4. Reuse: class declaration, method signature, other members   │
│                                                                  │
│  Result: Parse ~10 lines instead of ~10000                     │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### Incremental Algorithm

```rust
pub struct IncrementalParser {
    /// Previous parse result
    old_tree: GreenNode,
    
    /// The edit that was applied
    edit: TextEdit,
}

impl IncrementalParser {
    pub fn reparse(&self, new_text: &str) -> GreenNode {
        // Find the smallest subtree containing the edit
        let reparse_range = self.find_reparse_range();
        
        // Check if we can do incremental reparse
        if self.can_reparse_incrementally(&reparse_range) {
            self.reparse_incrementally(new_text, reparse_range)
        } else {
            // Fall back to full reparse
            Parser::new(new_text).parse()
        }
    }
    
    fn find_reparse_range(&self) -> ReparsableRange {
        // Walk tree to find smallest node containing edit
        let edit_start = self.edit.range.start;
        let edit_end = self.edit.range.end;
        
        // Find node that:
        // 1. Contains entire edit
        // 2. Is a "reparsable" boundary (statement, declaration, block)
        // 3. Is as small as possible
        
        self.find_containing_reparsable_node(edit_start, edit_end)
    }
    
    fn can_reparse_incrementally(&self, range: &ReparsableRange) -> bool {
        // Can reparse if:
        // 1. Edit is contained within a single reparsable unit
        // 2. Edit doesn't cross certain boundaries (e.g., string literals)
        // 3. Resulting text is likely parseable
        
        matches!(range.kind, 
            SyntaxKind::Block |
            SyntaxKind::Statement |
            SyntaxKind::Expression |
            SyntaxKind::MethodDeclaration
        )
    }
}
```

---

## Java-Specific Parsing Challenges

### Generics Ambiguity

```
┌─────────────────────────────────────────────────────────────────┐
│                    GENERICS AMBIGUITY                            │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  PROBLEM: < can mean different things                           │
│                                                                  │
│  List<String> x;        // < starts type argument               │
│  x < y                  // < is comparison operator             │
│  foo.<String>bar()      // < starts type argument in call       │
│  a<b, c>d               // Could be: (a<b), (c>d) or generic    │
│                                                                  │
│  SOLUTION: Context-aware parsing                                │
│                                                                  │
│  1. After type name in declaration context → type argument      │
│  2. After '.' before identifier → method type argument          │
│  3. In expression context → comparison (default)                │
│  4. Lookahead to disambiguate complex cases                     │
│                                                                  │
│  Implementation:                                                 │
│  fn parse_type_arguments_maybe(&mut self) -> Option<TypeArgs> { │
│    if !self.at(SyntaxKind::Less) {                             │
│      return None;                                               │
│    }                                                            │
│                                                                  │
│    // Try to parse as type arguments                            │
│    let checkpoint = self.checkpoint();                          │
│    if let Some(args) = self.try_parse_type_arguments() {       │
│      return Some(args);                                         │
│    }                                                            │
│                                                                  │
│    // Not type arguments, restore position                      │
│    self.restore(checkpoint);                                    │
│    None                                                         │
│  }                                                              │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### Lambda vs Cast Ambiguity

```
┌─────────────────────────────────────────────────────────────────┐
│                    LAMBDA AMBIGUITY                              │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  PROBLEM: Parenthesized expressions are ambiguous               │
│                                                                  │
│  (x) -> x + 1           // Lambda with parameter x              │
│  (x) + 1                // Cast (unlikely but legal)            │
│  (int x) -> x           // Lambda (clear)                       │
│  (x, y) -> x + y        // Lambda (clear - multiple params)     │
│  (Type) x               // Cast expression                      │
│                                                                  │
│  SOLUTION: Lookahead after closing paren                        │
│                                                                  │
│  1. See '(' in expression context                               │
│  2. Parse contents tentatively                                  │
│  3. After ')':                                                  │
│     - See '->' → definitely lambda                              │
│     - See identifier/literal → probably cast                    │
│     - Multiple comma-separated items → definitely lambda        │
│     - Typed parameter → definitely lambda                       │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### Contextual Keywords

```rust
/// Handle contextual keywords (identifiers that are keywords in some contexts)
impl Parser {
    fn parse_declaration(&mut self) {
        // 'var' is keyword only in local variable declarations
        if self.at_contextual_keyword("var") {
            self.parse_var_declaration();
        }
        // 'record' is keyword only at class declaration position
        else if self.at_contextual_keyword("record") {
            self.parse_record_declaration();
        }
        // 'sealed' only before class/interface
        else if self.at_contextual_keyword("sealed") {
            self.parse_sealed_class();
        }
        // ... etc
    }
    
    fn at_contextual_keyword(&self, kw: &str) -> bool {
        self.at(SyntaxKind::Identifier) && self.current_text() == kw
    }
}
```

---

## Typed Syntax Tree API

### Generated API

```rust
/// Strongly-typed wrapper for class declarations
#[derive(Debug, Clone)]
pub struct ClassDeclaration {
    syntax: SyntaxNode,
}

impl ClassDeclaration {
    pub fn modifiers(&self) -> impl Iterator<Item = Modifier> {
        self.syntax.children()
            .filter_map(Modifier::cast)
    }
    
    pub fn name(&self) -> Option<Identifier> {
        self.syntax.children()
            .find_map(Identifier::cast)
    }
    
    pub fn type_parameters(&self) -> Option<TypeParameterList> {
        self.syntax.children()
            .find_map(TypeParameterList::cast)
    }
    
    pub fn extends_clause(&self) -> Option<ExtendsClause> {
        self.syntax.children()
            .find_map(ExtendsClause::cast)
    }
    
    pub fn implements_clause(&self) -> Option<ImplementsClause> {
        self.syntax.children()
            .find_map(ImplementsClause::cast)
    }
    
    pub fn body(&self) -> Option<ClassBody> {
        self.syntax.children()
            .find_map(ClassBody::cast)
    }
}

/// Type-safe node casting
impl ClassDeclaration {
    pub fn cast(syntax: SyntaxNode) -> Option<Self> {
        if syntax.kind() == SyntaxKind::ClassDeclaration {
            Some(Self { syntax })
        } else {
            None
        }
    }
}
```

---

## Integration with Query System

```rust
/// Parsing query integration
#[query]
pub fn parse(db: &dyn Database, file: FileId) -> Arc<Parse> {
    let content = db.file_content(file);
    let parser = Parser::new(&content);
    Arc::new(parser.parse())
}

/// Incremental parsing with previous result
#[query]
pub fn parse_incremental(
    db: &dyn Database, 
    file: FileId,
    edit: TextEdit,
) -> Arc<Parse> {
    // Get previous parse result
    let old_parse = db.parse(file);
    
    // Apply incremental parsing
    let new_content = db.file_content(file);
    let parser = IncrementalParser::new(old_parse.tree(), edit);
    Arc::new(parser.reparse(&new_content))
}
```

---

## Performance Considerations

```
┌─────────────────────────────────────────────────────────────────┐
│                    PERFORMANCE TARGETS                           │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  FULL PARSE                                                      │
│  • Small file (100 lines): < 1ms                                │
│  • Medium file (1000 lines): < 10ms                             │
│  • Large file (10000 lines): < 100ms                            │
│                                                                  │
│  INCREMENTAL REPARSE                                            │
│  • Single character edit: < 0.5ms                               │
│  • Statement edit: < 1ms                                        │
│  • Block edit: < 5ms                                            │
│                                                                  │
│  MEMORY                                                          │
│  • Green tree: ~50 bytes per node                               │
│  • Token: ~24 bytes                                             │
│  • 10K line file: ~2MB for full tree                            │
│                                                                  │
│  OPTIMIZATIONS                                                   │
│  • Arena allocation for green nodes                             │
│  • String interning for identifiers                             │
│  • Lazy child expansion                                         │
│  • Memory-mapped file reading                                   │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

---

## Next Steps

1. → [Semantic Analysis](06-semantic-analysis.md): How semantic analysis builds on syntax trees
2. → [Code Intelligence](07-code-intelligence.md): How parsing enables IDE features

---

[← Previous: Incremental Computation](04-incremental-computation.md) | [Next: Semantic Analysis →](06-semantic-analysis.md)
