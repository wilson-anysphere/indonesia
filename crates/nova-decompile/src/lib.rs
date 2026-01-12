//! Classfile-to-source decompilation fallback.
//!
//! When source jars/sources are unavailable, Nova synthesizes Java-like text from
//! a `.class` file so "go to definition", hover, and signature help can still
//! show something meaningful.

use nova_cache::{DerivedArtifactCache, Fingerprint};
use nova_classfile::{Annotation, ClassFile, ClassStub, FieldStub, MethodStub};
use nova_core::{Position, Range};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

mod document_store;

pub use document_store::{
    DecompiledDocumentStore, DecompiledStoreGcPolicy, DecompiledStoreGcReport,
};

/// URI scheme used by Nova for all virtual documents (ADR0006).
///
/// Nova synthesizes some documents that do not exist on disk (e.g. decompiled
/// `.class` stubs). These are addressed using `nova:///...` URIs so they can be
/// routed through the rest of the system like normal files without colliding
/// with on-disk paths.
pub const NOVA_VIRTUAL_URI_SCHEME: &str = "nova";

/// Version of the decompiled-URI hashing schema.
///
/// Bump this when the decompiler output format changes in a way that should
/// invalidate cached/stored virtual documents (e.g. signature rendering tweaks,
/// annotation formatting changes, etc). The version is incorporated into the
/// content hash used in `nova:///decompiled/<hash>/...` URIs.
pub const DECOMPILER_SCHEMA_VERSION: u32 = 1;

/// Legacy URI scheme for decompiled virtual documents.
///
/// This pre-dates ADR0006 and does **not** incorporate a content hash. It is
/// retained for backwards compatibility with downstream crates.
pub const DECOMPILE_URI_SCHEME: &str = "nova-decompile";
const DECOMPILE_QUERY_SCHEMA_VERSION: u32 = 1;

pub type Result<T> = std::result::Result<T, DecompileError>;

#[derive(Debug, thiserror::Error)]
pub enum DecompileError {
    #[error("classfile error: {0}")]
    ClassFile(#[from] nova_classfile::Error),
    #[error("cache error: {0}")]
    Cache(#[from] nova_cache::CacheError),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SymbolKey {
    Class { internal_name: String },
    Field { name: String, descriptor: String },
    Method { name: String, descriptor: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolRange {
    pub symbol: SymbolKey,
    pub range: Range,
}

#[derive(Debug, Clone)]
pub struct DecompiledClass {
    pub text: String,
    pub mappings: Vec<SymbolRange>,
    pub from_cache: bool,
}

impl DecompiledClass {
    pub fn range_for(&self, symbol: &SymbolKey) -> Option<Range> {
        self.mappings
            .iter()
            .find(|m| &m.symbol == symbol)
            .map(|m| m.range)
    }
}

/// Decompiles a `.class` file into Java-like stub source.
pub fn decompile_classfile(bytes: &[u8]) -> Result<DecompiledClass> {
    decompile_impl(bytes, None)
}

/// Decompiles a `.class` file and caches the output via `cache`.
pub fn decompile_classfile_cached(
    bytes: &[u8],
    cache: &DerivedArtifactCache,
) -> Result<DecompiledClass> {
    decompile_impl(bytes, Some(cache))
}

fn decompile_impl(bytes: &[u8], cache: Option<&DerivedArtifactCache>) -> Result<DecompiledClass> {
    let fingerprint = Fingerprint::from_bytes(bytes);

    if let Some(cache) = cache {
        let args = DecompileArgs {};
        let mut inputs = BTreeMap::new();
        inputs.insert("classfile".to_string(), fingerprint.clone());

        if let Some(hit) = cache.load::<CachedDecompile>(
            "nova-decompile",
            DECOMPILE_QUERY_SCHEMA_VERSION,
            &args,
            &inputs,
        )? {
            return Ok(hit.into_decompiled(true));
        }

        let fresh = decompile_uncached(bytes)?;
        let cached = CachedDecompile::from_decompiled(&fresh);
        cache.store(
            "nova-decompile",
            DECOMPILE_QUERY_SCHEMA_VERSION,
            &args,
            &inputs,
            &cached,
        )?;
        return Ok(cached.into_decompiled(false));
    }

    decompile_uncached(bytes)
}

fn decompile_uncached(bytes: &[u8]) -> Result<DecompiledClass> {
    let classfile = ClassFile::parse(bytes)?;
    let stub = classfile.stub()?;
    let (text, mappings) = decompile_stub(&stub);
    Ok(DecompiledClass {
        text,
        mappings,
        from_cache: false,
    })
}

#[derive(Debug, Serialize)]
struct DecompileArgs {}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedDecompile {
    text: String,
    mappings: Vec<CachedSymbolRange>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedSymbolRange {
    symbol: SymbolKey,
    start_line: u32,
    start_character: u32,
    end_line: u32,
    end_character: u32,
}

impl CachedDecompile {
    fn from_decompiled(value: &DecompiledClass) -> Self {
        Self {
            text: value.text.clone(),
            mappings: value
                .mappings
                .iter()
                .map(|m| CachedSymbolRange {
                    symbol: m.symbol.clone(),
                    start_line: m.range.start.line,
                    start_character: m.range.start.character,
                    end_line: m.range.end.line,
                    end_character: m.range.end.character,
                })
                .collect(),
        }
    }

    fn into_decompiled(self, from_cache: bool) -> DecompiledClass {
        DecompiledClass {
            text: self.text,
            mappings: self
                .mappings
                .into_iter()
                .map(|m| SymbolRange {
                    symbol: m.symbol,
                    range: Range::new(
                        Position::new(m.start_line, m.start_character),
                        Position::new(m.end_line, m.end_character),
                    ),
                })
                .collect(),
            from_cache,
        }
    }
}

fn decompile_stub(stub: &ClassStub) -> (String, Vec<SymbolRange>) {
    let internal_name = stub.internal_name.as_str();
    let (package_internal, simple_name) = split_internal_name(internal_name);
    let package_dot = package_internal.replace('/', ".");

    let mut w = TextWriter::new();
    let mut mappings = Vec::new();

    if !package_internal.is_empty() {
        w.push_str("package ");
        w.push_str(&package_dot);
        w.push_str(";\n\n");
    }

    for ann in collect_annotation_names(&stub.annotations, &package_dot) {
        w.push_str("@");
        w.push_str(&ann);
        w.push_str("\n");
    }

    let kind = class_kind(stub.access_flags);
    let modifiers = class_modifiers(stub.access_flags, kind);
    if !modifiers.is_empty() {
        w.push_str(&modifiers);
        w.push_str(" ");
    }
    w.push_str(kind);
    w.push_str(" ");

    let start = w.position();
    w.push_str(&simple_name);
    let end = w.position();
    mappings.push(SymbolRange {
        symbol: SymbolKey::Class {
            internal_name: stub.internal_name.clone(),
        },
        range: Range::new(start, end),
    });

    match kind {
        "class" => {
            if let Some(super_class) = stub.super_class.as_deref() {
                if super_class != "java/lang/Object" {
                    w.push_str(" extends ");
                    w.push_str(&format_type_name(super_class, &package_dot));
                }
            }
            if !stub.interfaces.is_empty() {
                w.push_str(" implements ");
                for (i, iface) in stub.interfaces.iter().enumerate() {
                    if i != 0 {
                        w.push_str(", ");
                    }
                    w.push_str(&format_type_name(iface, &package_dot));
                }
            }
        }
        "enum" => {
            if !stub.interfaces.is_empty() {
                w.push_str(" implements ");
                for (i, iface) in stub.interfaces.iter().enumerate() {
                    if i != 0 {
                        w.push_str(", ");
                    }
                    w.push_str(&format_type_name(iface, &package_dot));
                }
            }
        }
        "interface" | "@interface" => {
            if !stub.interfaces.is_empty() {
                w.push_str(" extends ");
                for (i, iface) in stub.interfaces.iter().enumerate() {
                    if i != 0 {
                        w.push_str(", ");
                    }
                    w.push_str(&format_type_name(iface, &package_dot));
                }
            }
        }
        _ => {}
    }

    w.push_str(" {\n");

    for field in &stub.fields {
        write_member_annotations(
            &mut w,
            &collect_annotation_names(&field.annotations, &package_dot),
            1,
        );
        write_indent(&mut w, 1);
        write_field_stub(&mut w, field, &package_dot, &mut mappings);
        w.push_str("\n");
    }

    if !stub.fields.is_empty() && !stub.methods.is_empty() {
        w.push_str("\n");
    }

    for method in &stub.methods {
        if method.name == "<clinit>" {
            continue;
        }
        write_member_annotations(
            &mut w,
            &collect_annotation_names(&method.annotations, &package_dot),
            1,
        );
        write_indent(&mut w, 1);
        write_method_stub(
            &mut w,
            method,
            &simple_name,
            &package_dot,
            kind,
            &mut mappings,
        );
        w.push_str("\n");
    }

    w.push_str("}\n");
    (w.finish(), mappings)
}

fn write_field_stub(
    w: &mut TextWriter,
    field: &FieldStub,
    package_dot: &str,
    mappings: &mut Vec<SymbolRange>,
) {
    let modifiers = field_modifiers(field.access_flags);
    let ty = format_field_type(&field.parsed_descriptor, package_dot);

    if !modifiers.is_empty() {
        w.push_str(&modifiers);
        w.push_str(" ");
    }
    w.push_str(&ty);
    w.push_str(" ");

    let start = w.position();
    w.push_str(&field.name);
    let end = w.position();
    mappings.push(SymbolRange {
        symbol: SymbolKey::Field {
            name: field.name.clone(),
            descriptor: field.descriptor.clone(),
        },
        range: Range::new(start, end),
    });

    w.push_str(";");
}

fn write_method_stub(
    w: &mut TextWriter,
    method: &MethodStub,
    class_simple_name: &str,
    package_dot: &str,
    class_kind: &str,
    mappings: &mut Vec<SymbolRange>,
) {
    let modifiers = method_modifiers(method.access_flags, class_kind);
    let is_ctor = method.name == "<init>";

    if !modifiers.is_empty() {
        w.push_str(&modifiers);
        w.push_str(" ");
    }

    if !is_ctor {
        w.push_str(&format_return_type(
            &method.parsed_descriptor.return_type,
            package_dot,
        ));
        w.push_str(" ");
    }

    let display_name = if is_ctor {
        class_simple_name
    } else {
        method.name.as_str()
    };
    let start = w.position();
    w.push_str(display_name);
    let end = w.position();

    mappings.push(SymbolRange {
        symbol: SymbolKey::Method {
            name: method.name.clone(),
            descriptor: method.descriptor.clone(),
        },
        range: Range::new(start, end),
    });

    w.push_str("(");
    for (i, param) in method.parsed_descriptor.params.iter().enumerate() {
        if i != 0 {
            w.push_str(", ");
        }
        w.push_str(&format_field_type(param, package_dot));
        w.push_str(" ");
        w.push_str(&format!("arg{i}"));
    }
    w.push_str(")");

    if method.access_flags & ACC_ABSTRACT != 0 || method.access_flags & ACC_NATIVE != 0 {
        w.push_str(";");
    } else {
        w.push_str(" { /* compiled code */ }");
    }
}

fn write_member_annotations(w: &mut TextWriter, annotations: &[String], indent: usize) {
    for ann in annotations {
        write_indent(w, indent);
        w.push_str("@");
        w.push_str(ann);
        w.push_str("\n");
    }
}

fn write_indent(w: &mut TextWriter, indent: usize) {
    for _ in 0..indent {
        w.push_str("    ");
    }
}

fn split_internal_name(internal: &str) -> (&str, String) {
    let (pkg, raw_name) = match internal.rsplit_once('/') {
        Some((pkg, name)) => (pkg, name),
        None => ("", internal),
    };

    // For inner classes, use the inner simple name when generating a standalone
    // stub (otherwise the source identifier would contain `$`).
    let simple = raw_name
        .rsplit_once('$')
        .map(|(_, inner)| inner)
        .unwrap_or(raw_name);
    (pkg, simple.to_string())
}

fn class_kind(access_flags: u16) -> &'static str {
    if access_flags & ACC_ANNOTATION != 0 {
        "@interface"
    } else if access_flags & ACC_INTERFACE != 0 {
        "interface"
    } else if access_flags & ACC_ENUM != 0 {
        "enum"
    } else {
        "class"
    }
}

fn class_modifiers(access_flags: u16, kind: &str) -> String {
    let mut mods = Vec::new();
    if access_flags & ACC_PUBLIC != 0 {
        mods.push("public");
    } else if access_flags & ACC_PROTECTED != 0 {
        mods.push("protected");
    } else if access_flags & ACC_PRIVATE != 0 {
        mods.push("private");
    }

    if access_flags & ACC_ABSTRACT != 0 && kind != "interface" && kind != "@interface" {
        mods.push("abstract");
    }
    if access_flags & ACC_FINAL != 0 && kind != "enum" && kind != "@interface" {
        mods.push("final");
    }
    mods.join(" ")
}

fn field_modifiers(access_flags: u16) -> String {
    let mut mods = Vec::new();
    if access_flags & ACC_PUBLIC != 0 {
        mods.push("public");
    } else if access_flags & ACC_PROTECTED != 0 {
        mods.push("protected");
    } else if access_flags & ACC_PRIVATE != 0 {
        mods.push("private");
    }
    if access_flags & ACC_STATIC != 0 {
        mods.push("static");
    }
    if access_flags & ACC_FINAL != 0 {
        mods.push("final");
    }
    if access_flags & ACC_TRANSIENT != 0 {
        mods.push("transient");
    }
    if access_flags & ACC_VOLATILE != 0 {
        mods.push("volatile");
    }
    mods.join(" ")
}

fn method_modifiers(access_flags: u16, class_kind: &str) -> String {
    let mut mods = Vec::new();
    if access_flags & ACC_PUBLIC != 0 {
        mods.push("public");
    } else if access_flags & ACC_PROTECTED != 0 {
        mods.push("protected");
    } else if access_flags & ACC_PRIVATE != 0 {
        mods.push("private");
    } else if class_kind == "interface" || class_kind == "@interface" {
        mods.push("public");
    }
    if access_flags & ACC_STATIC != 0 {
        mods.push("static");
    }
    if access_flags & ACC_FINAL != 0 {
        mods.push("final");
    }
    if access_flags & ACC_ABSTRACT != 0 {
        mods.push("abstract");
    }
    if access_flags & ACC_SYNCHRONIZED != 0 {
        mods.push("synchronized");
    }
    if access_flags & ACC_NATIVE != 0 {
        mods.push("native");
    }
    if access_flags & ACC_STRICT != 0 {
        mods.push("strictfp");
    }
    mods.join(" ")
}

fn collect_annotation_names(annotations: &[Annotation], package_dot: &str) -> Vec<String> {
    let mut names: Vec<String> = annotations
        .iter()
        .filter_map(|ann| {
            let internal = ann
                .type_internal_name
                .as_deref()
                .map(|s| s.to_string())
                .or_else(|| descriptor_to_internal_name(&ann.type_descriptor))?;
            Some(format_type_name(&internal, package_dot))
        })
        .collect();

    names.sort();
    names.dedup();

    if let Some(pos) = names.iter().position(|n| n == "Deprecated") {
        let deprecated = names.remove(pos);
        names.insert(0, deprecated);
    }

    names
}

fn descriptor_to_internal_name(desc: &str) -> Option<String> {
    let inner = desc.strip_prefix('L')?.strip_suffix(';')?;
    Some(inner.to_string())
}

fn format_field_type(ty: &nova_classfile::FieldType, package_dot: &str) -> String {
    use nova_classfile::{BaseType, FieldType};
    match ty {
        FieldType::Base(b) => match b {
            BaseType::Byte => "byte".to_string(),
            BaseType::Char => "char".to_string(),
            BaseType::Double => "double".to_string(),
            BaseType::Float => "float".to_string(),
            BaseType::Int => "int".to_string(),
            BaseType::Long => "long".to_string(),
            BaseType::Short => "short".to_string(),
            BaseType::Boolean => "boolean".to_string(),
        },
        FieldType::Object(internal) => format_type_name(internal, package_dot),
        FieldType::Array(component) => format!("{}[]", format_field_type(component, package_dot)),
    }
}

fn format_return_type(ty: &nova_classfile::ReturnType, package_dot: &str) -> String {
    match ty {
        nova_classfile::ReturnType::Void => "void".to_string(),
        nova_classfile::ReturnType::Type(field) => format_field_type(field, package_dot),
    }
}

fn format_type_name(internal: &str, package_dot: &str) -> String {
    let dot = internal.replace(['/', '$'], ".");
    if let Some(rest) = dot.strip_prefix("java.lang.") {
        rest.to_string()
    } else if !package_dot.is_empty() {
        let prefix = format!("{package_dot}.");
        if dot.starts_with(&prefix) {
            dot[prefix.len()..].to_string()
        } else {
            dot
        }
    } else {
        dot
    }
}

struct TextWriter {
    text: String,
    line: u32,
    col: u32,
}

impl TextWriter {
    fn new() -> Self {
        Self {
            text: String::new(),
            line: 0,
            col: 0,
        }
    }

    fn position(&self) -> Position {
        Position::new(self.line, self.col)
    }

    fn push_str(&mut self, s: &str) {
        self.text.push_str(s);
        for ch in s.chars() {
            if ch == '\n' {
                self.line += 1;
                self.col = 0;
            } else {
                self.col += ch.len_utf16() as u32;
            }
        }
    }

    fn finish(self) -> String {
        self.text
    }
}

/// Returns a stable URI for a decompiled classfile (legacy scheme).
///
/// Example: `nova-decompile:///com/example/Foo.class`
///
/// This helper remains for backwards compatibility; new virtual documents
/// should prefer [`decompiled_uri_for_classfile`], which produces the canonical
/// `nova:///decompiled/<hash>/<binary-name>.java` URI format.
pub fn uri_for_class_internal_name(internal_name: &str) -> String {
    let internal_name = normalize_legacy_internal_name(internal_name);
    format!("{DECOMPILE_URI_SCHEME}:///{}.class", internal_name)
}

/// Attempts to extract the class internal name from a decompiled URI.
pub fn class_internal_name_from_uri(uri: &str) -> Option<String> {
    if uri.contains('?') || uri.contains('#') {
        return None;
    }
    let rest = uri.strip_prefix(DECOMPILE_URI_SCHEME)?.strip_prefix(':')?;

    // Extract the path component, rejecting URIs with a non-empty authority.
    // Mirror `nova-vfs` legacy decompile URI parsing so downstream sees the same internal name.
    let path = if let Some(after_slashes) = rest.strip_prefix("//") {
        if !after_slashes.starts_with('/') {
            return None;
        }
        after_slashes
    } else if rest.starts_with('/') {
        rest
    } else {
        return None;
    };

    let path = path.trim_matches(|c| c == '/' || c == '\\');
    if path.is_empty() {
        return None;
    }

    let path = if path.contains('\\') {
        std::borrow::Cow::Owned(path.replace('\\', "/"))
    } else {
        std::borrow::Cow::Borrowed(path)
    };

    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if segments.is_empty() {
        return None;
    }
    if segments.contains(&"..") {
        return None;
    }

    let last = segments.last()?;
    let stem = last.strip_suffix(".class")?;
    if stem.is_empty() {
        return None;
    }

    let mut internal = String::new();
    for (idx, seg) in segments.iter().enumerate() {
        if idx > 0 {
            internal.push('/');
        }
        if idx + 1 == segments.len() {
            internal.push_str(stem);
        } else {
            internal.push_str(seg);
        }
    }

    Some(internal)
}

fn normalize_legacy_internal_name(internal_name: &str) -> String {
    let internal_name = internal_name
        .strip_suffix(".class")
        .unwrap_or(internal_name);
    let internal_name = internal_name.trim_matches(|c| c == '/' || c == '\\');
    let internal_name = if internal_name.contains('\\') {
        internal_name.replace('\\', "/")
    } else {
        internal_name.to_string()
    };

    let segments: Vec<&str> = internal_name.split('/').filter(|s| !s.is_empty()).collect();
    segments.join("/")
}

/// Parsed representation of a canonical decompiled virtual-document URI.
///
/// Canonical decompiled URIs have the form:
/// `nova:///decompiled/<content-hash>/<binary-name>.java`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedDecompiledUri {
    /// Hash of the original `.class` bytes plus [`DECOMPILER_SCHEMA_VERSION`].
    ///
    /// This is a lowercase SHA-256 hex digest.
    pub content_hash: String,
    /// JVM binary name for the class (dotted package, `$` for nested classes).
    pub binary_name: String,
}

impl ParsedDecompiledUri {
    /// Convert the binary name (`com.example.Foo$Inner`) back into a JVM internal
    /// name (`com/example/Foo$Inner`).
    pub fn internal_name(&self) -> String {
        self.binary_name.replace('.', "/")
    }
}

fn normalize_decompiled_binary_name(binary_name: &str) -> String {
    // Keep this logic in sync with `nova-vfs/src/path.rs:normalize_decompiled_binary_name`.
    let binary_name = binary_name
        .strip_suffix(".java")
        .unwrap_or(binary_name)
        .replace(['\\', '/'], ".")
        .trim_matches('.')
        .to_string();

    let mut out = String::with_capacity(binary_name.len());
    let mut last_dot = false;
    for ch in binary_name.chars() {
        if ch == '.' {
            if last_dot {
                continue;
            }
            last_dot = true;
            out.push('.');
        } else {
            last_dot = false;
            out.push(ch);
        }
    }

    out
}

fn decompiled_content_fingerprint(bytes: &[u8], schema_version: u32) -> Fingerprint {
    // Domain-separate the hash so it can't collide with other uses of
    // `Fingerprint::from_bytes` across the codebase.
    let mut input = Vec::with_capacity(
        b"nova-decompile\0".len() + std::mem::size_of::<u32>() + 1 + bytes.len(),
    );
    input.extend_from_slice(b"nova-decompile\0");
    input.extend_from_slice(&schema_version.to_le_bytes());
    input.extend_from_slice(b"\0");
    input.extend_from_slice(bytes);
    Fingerprint::from_bytes(input)
}

/// Returns the canonical ADR0006 virtual-document URI for a decompiled class.
///
/// Format: `nova:///decompiled/<content-hash>/<binary-name>.java`
///
/// *Why include a hash?*
/// The legacy `nova-decompile:///...` scheme only includes the class internal
/// name, which is not unique across jars/classpaths (e.g. two different
/// `com/example/Foo.class` versions). Content-addressing ensures the URI is
/// stable for identical bytecode and changes whenever the bytecode (or
/// [`DECOMPILER_SCHEMA_VERSION`]) changes.
///
/// *Why the `.java` extension?*
/// Many editor integrations key language mode/formatting off the path
/// extension; using `.java` ensures decompiled stubs are treated as Java source.
pub fn decompiled_uri_for_classfile(bytes: &[u8], internal_name: &str) -> String {
    let fingerprint = decompiled_content_fingerprint(bytes, DECOMPILER_SCHEMA_VERSION);
    let binary_name = normalize_decompiled_binary_name(internal_name);
    format!("{NOVA_VIRTUAL_URI_SCHEME}:///decompiled/{fingerprint}/{binary_name}.java")
}

/// Attempts to parse a canonical decompiled virtual-document URI.
pub fn parse_decompiled_uri(uri: &str) -> Option<ParsedDecompiledUri> {
    // Canonical form does not include query/fragment (ADR0006).
    if uri.contains('?') || uri.contains('#') {
        return None;
    }

    let prefix = format!("{NOVA_VIRTUAL_URI_SCHEME}:///decompiled/");
    let rest = uri.strip_prefix(&prefix)?;
    let (content_hash, filename) = rest.split_once('/')?;

    if content_hash.is_empty() {
        return None;
    }

    // Avoid parsing unrelated `nova:///decompiled/...` URIs.
    //
    // Canonical content hashes are lowercase hex (as produced by
    // `nova_cache::Fingerprint`), but accept uppercase hex and normalize so
    // downstream always sees a single canonical representation.
    if content_hash.len() != 64 || !content_hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let content_hash = content_hash.to_ascii_lowercase();

    let binary_name = filename.strip_suffix(".java")?;
    if binary_name.is_empty() || binary_name.contains('/') {
        return None;
    }

    let binary_name = normalize_decompiled_binary_name(binary_name);
    if binary_name.is_empty() {
        return None;
    }

    Some(ParsedDecompiledUri {
        content_hash,
        binary_name,
    })
}

/// Returns the canonical ADR0006 `nova:///decompiled/<hash>/<binary-name>.java` URI for `uri`.
///
/// This is a small compatibility shim for callers that still produce legacy
/// `nova-decompile:///...` URIs.
///
/// - If `uri` is already a canonical `nova:///decompiled/...` URI, it is returned in
///   canonicalized form (the hash and binary name may be normalized).
/// - If `uri` is a legacy `nova-decompile:///...` URI, it is upgraded to the canonical
///   URI using `classfile_bytes` to compute the content hash.
pub fn canonicalize_decompiled_uri(uri: &str, classfile_bytes: &[u8]) -> Option<String> {
    if let Some(parsed) = parse_decompiled_uri(uri) {
        return Some(format!(
            "{NOVA_VIRTUAL_URI_SCHEME}:///decompiled/{}/{}.java",
            parsed.content_hash, parsed.binary_name
        ));
    }
    let internal = class_internal_name_from_uri(uri)?;
    let upgraded = decompiled_uri_for_classfile(classfile_bytes, &internal);
    let parsed = parse_decompiled_uri(&upgraded)?;
    Some(format!(
        "{NOVA_VIRTUAL_URI_SCHEME}:///decompiled/{}/{}.java",
        parsed.content_hash, parsed.binary_name
    ))
}

// Access flag constants (subset used by the stub generator).
const ACC_PUBLIC: u16 = 0x0001;
const ACC_PRIVATE: u16 = 0x0002;
const ACC_PROTECTED: u16 = 0x0004;
const ACC_STATIC: u16 = 0x0008;
const ACC_FINAL: u16 = 0x0010;
const ACC_VOLATILE: u16 = 0x0040;
const ACC_TRANSIENT: u16 = 0x0080;
const ACC_SYNCHRONIZED: u16 = 0x0020;
const ACC_NATIVE: u16 = 0x0100;
const ACC_INTERFACE: u16 = 0x0200;
const ACC_ABSTRACT: u16 = 0x0400;
const ACC_STRICT: u16 = 0x0800;
const ACC_ANNOTATION: u16 = 0x2000;
const ACC_ENUM: u16 = 0x4000;
