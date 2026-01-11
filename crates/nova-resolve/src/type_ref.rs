//! Lightweight parser for Java type references.
//!
//! This module is intentionally small and best-effort: it is used by IDE-facing
//! layers that need to turn textual type references (as they appear in source)
//! into a structural [`nova_types::Type`] so generics/arrays/wildcards are not
//! lost.
//!
//! The parser performs name resolution via [`crate::Resolver`] + scope graph so
//! that `String` becomes `java.lang.String`, `Map.Entry` resolves through an
//! imported `Map`, etc.
//!
//! # Supported grammar (best-effort)
//! - primitives + `void`
//! - simple/qualified names (`String`, `java.util.List`, `Map.Entry`)
//! - generic args: `Foo<Bar, Baz>`
//! - wildcards: `?`, `? extends T`, `? super T`
//! - arrays: suffix `[]` repeated
//!
//! The parser is whitespace-tolerant and returns [`Type::Unknown`] on malformed
//! input rather than erroring.

use nova_core::QualifiedName;
use nova_types::{ClassDef, ClassId, ClassKind, PrimitiveType, Type, WildcardBound};

use crate::{Resolver, ScopeGraph, ScopeId};

/// Interns/looks up classes by binary name so the parser can always produce a
/// [`Type::Class`], even for types that are not yet fully modelled by the
/// database.
pub trait ClassInterner {
    fn intern_class(&mut self, binary_name: &str) -> ClassId;
}

impl ClassInterner for nova_types::TypeStore {
    fn intern_class(&mut self, binary_name: &str) -> ClassId {
        if let Some(id) = self.class_id(binary_name) {
            return id;
        }

        // Best-effort: create a stub class definition so we can refer to it via
        // `Type::Class`. The semantic/type-checking layers treat missing class
        // information gracefully, so this is safe for IDE features.
        self.add_class(ClassDef {
            name: binary_name.to_string(),
            kind: ClassKind::Class,
            type_params: Vec::new(),
            super_class: None,
            interfaces: Vec::new(),
            fields: Vec::new(),
            constructors: Vec::new(),
            methods: Vec::new(),
        })
    }
}

/// Parse a Java type reference using `resolver` for name resolution.
///
/// `scope` should be the scope in which the type is written (so imports and
/// enclosing types are considered).
///
/// This function is best-effort and returns [`Type::Unknown`] on invalid input.
pub fn parse_type_ref<I: ClassInterner>(
    text: &str,
    resolver: &Resolver<'_>,
    scopes: &ScopeGraph,
    scope: ScopeId,
    interner: &mut I,
) -> Type {
    let mut p = Parser {
        input: text,
        bytes: text.as_bytes(),
        pos: 0,
        resolver,
        scopes,
        scope,
        interner,
    };

    let ty = p.parse_type();
    // Consume trailing whitespace to avoid surprising `Unknown` on well-formed
    // types with trailing spaces.
    p.skip_ws();
    if p.pos < p.bytes.len() {
        // Best-effort: ignore trailing garbage.
        return ty;
    }
    ty
}

struct Parser<'a, 'r, I> {
    input: &'a str,
    bytes: &'a [u8],
    pos: usize,
    resolver: &'r Resolver<'r>,
    scopes: &'r ScopeGraph,
    scope: ScopeId,
    interner: &'r mut I,
}

impl<'a, 'r, I: ClassInterner> Parser<'a, 'r, I> {
    fn parse_type(&mut self) -> Type {
        self.skip_ws();

        let mut ty = if self.peek_byte() == Some(b'?') {
            self.parse_wildcard()
        } else {
            self.parse_named_or_primitive()
        };

        ty = self.parse_array_suffix(ty);
        ty
    }

    fn parse_wildcard(&mut self) -> Type {
        self.bump(); // '?'
        self.skip_ws();

        if self.eat_keyword("extends") {
            let bound = self.parse_type();
            return Type::Wildcard(WildcardBound::Extends(Box::new(bound)));
        }

        if self.eat_keyword("super") {
            let bound = self.parse_type();
            return Type::Wildcard(WildcardBound::Super(Box::new(bound)));
        }

        Type::Wildcard(WildcardBound::Unbounded)
    }

    fn parse_named_or_primitive(&mut self) -> Type {
        let Some(path) = self.parse_qualified_ident() else {
            return Type::Unknown;
        };

        // Primitive / void are always a single token (no dots).
        if !path.contains('.') {
            if let Some(prim) = primitive_from_str(path) {
                return Type::Primitive(prim);
            }
            if path == "void" {
                return Type::Void;
            }
        }

        let mut ty = self.resolve_class_type(path, Vec::new());

        self.skip_ws();
        if self.peek_byte() == Some(b'<') {
            let args = self.parse_type_arguments();
            if let Type::Class(class) = &mut ty {
                class.args = args;
            }
        }

        ty
    }

    fn resolve_class_type(&mut self, path: &str, args: Vec<Type>) -> Type {
        let qn = QualifiedName::from_dotted(path);

        let class_id = if let Some(resolved) =
            self.resolver
                .resolve_qualified_type_in_scope(self.scopes, self.scope, &qn)
        {
            self.interner.intern_class(resolved.as_str())
        } else {
            // Best-effort: keep the textual name if resolution fails.
            self.interner.intern_class(path)
        };

        Type::class(class_id, args)
    }

    fn parse_type_arguments(&mut self) -> Vec<Type> {
        if !self.eat_byte(b'<') {
            return Vec::new();
        }

        let mut args = Vec::new();
        loop {
            self.skip_ws();
            if self.eat_byte(b'>') {
                break;
            }

            args.push(self.parse_type());
            self.skip_ws();

            if self.eat_byte(b',') {
                continue;
            }

            // End of args list (best-effort).
            self.skip_ws();
            let _ = self.eat_byte(b'>');
            break;
        }

        args
    }

    fn parse_array_suffix(&mut self, mut ty: Type) -> Type {
        loop {
            self.skip_ws();
            if !self.eat_byte(b'[') {
                break;
            }
            self.skip_ws();
            if !self.eat_byte(b']') {
                break;
            }
            ty = Type::Array(Box::new(ty));
        }
        ty
    }

    fn parse_qualified_ident(&mut self) -> Option<&'a str> {
        self.skip_ws();
        let start = self.pos;

        if !self.peek_byte().is_some_and(is_ident_start) {
            return None;
        }

        self.bump();
        while self.peek_byte().is_some_and(is_ident_continue) {
            self.bump();
        }

        loop {
            if self.peek_byte() != Some(b'.') {
                break;
            }

            // Lookahead: require an identifier after the dot.
            let dot = self.pos;
            self.bump(); // '.'

            if !self.peek_byte().is_some_and(is_ident_start) {
                // Roll back if this isn't actually a qualified identifier.
                self.pos = dot;
                break;
            }

            self.bump();
            while self.peek_byte().is_some_and(is_ident_continue) {
                self.bump();
            }
        }

        Some(&self.input[start..self.pos])
    }

    fn skip_ws(&mut self) {
        while self.peek_byte().is_some_and(|b| b.is_ascii_whitespace()) {
            self.pos += 1;
        }
    }

    fn peek_byte(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.peek_byte()?;
        self.pos += 1;
        Some(b)
    }

    fn eat_byte(&mut self, expected: u8) -> bool {
        if self.peek_byte() == Some(expected) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn eat_keyword(&mut self, kw: &str) -> bool {
        let rest = &self.input[self.pos..];
        if !rest.starts_with(kw) {
            return false;
        }

        let end = self.pos + kw.len();
        let boundary_ok =
            end >= self.bytes.len() || !self.bytes.get(end).copied().is_some_and(is_ident_continue);
        if !boundary_ok {
            return false;
        }

        self.pos = end;
        self.skip_ws();
        true
    }
}

fn is_ident_start(b: u8) -> bool {
    matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'_' | b'$')
}

fn is_ident_continue(b: u8) -> bool {
    is_ident_start(b) || matches!(b, b'0'..=b'9')
}

fn primitive_from_str(s: &str) -> Option<PrimitiveType> {
    Some(match s {
        "boolean" => PrimitiveType::Boolean,
        "byte" => PrimitiveType::Byte,
        "short" => PrimitiveType::Short,
        "char" => PrimitiveType::Char,
        "int" => PrimitiveType::Int,
        "long" => PrimitiveType::Long,
        "float" => PrimitiveType::Float,
        "double" => PrimitiveType::Double,
        _ => return None,
    })
}
