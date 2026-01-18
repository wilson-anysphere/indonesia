use std::env;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, Context, Result};

pub fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    let cmd = args
        .next()
        .ok_or_else(|| anyhow!("expected a command (try `codegen` or `syntax-lint`)"))?;

    match cmd.as_str() {
        "codegen" => {
            if let Some(arg) = args.next() {
                return Err(anyhow!(
                    "unexpected argument `{arg}` (usage: cargo xtask codegen)"
                ));
            }
            codegen()?;
        }
        "syntax-lint" => {
            if let Some(arg) = args.next() {
                return Err(anyhow!(
                    "unexpected argument `{arg}` (usage: cargo xtask syntax-lint)"
                ));
            }
            syntax_lint()?;
        }
        _ => {
            return Err(anyhow!(
                "unknown command `{cmd}` (supported: `codegen`, `syntax-lint`)"
            ));
        }
    }

    Ok(())
}

pub fn codegen() -> Result<()> {
    let repo_root = repo_root()?;

    let grammar_path = repo_root.join("crates/nova-syntax/grammar/java.syntax");
    let ast_out_path = repo_root.join("crates/nova-syntax/src/ast/generated.rs");

    let code = generate_ast(&grammar_path)?;
    write_if_changed(&ast_out_path, &code)?;

    Ok(())
}

pub fn syntax_lint() -> Result<()> {
    let repo_root = repo_root()?;
    let report = syntax_lint_report(&repo_root)?;
    println!("{report}");
    if report.is_clean() {
        Ok(())
    } else {
        Err(anyhow!("syntax-lint failed"))
    }
}

fn repo_root() -> Result<PathBuf> {
    // `cargo run --locked -p xtask -- ...` is executed with the workspace root as CWD.
    // Keep this helper anyway so tests can call into codegen from arbitrary CWDs.
    let cwd = env::current_dir().context("failed to read current working directory")?;
    Ok(cwd)
}

fn write_if_changed(path: &Path, contents: &str) -> Result<()> {
    let existing = match std::fs::read_to_string(path) {
        Ok(existing) => Some(existing),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => {
            return Err(err).with_context(|| format!("failed to read `{}`", path.display()));
        }
    };
    if existing.as_deref() == Some(contents) {
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create output directory `{}`", parent.display()))?;
    }

    std::fs::write(path, contents)
        .with_context(|| format!("failed to write `{}`", path.display()))?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cardinality {
    One,
    Optional,
    Many,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Field {
    name: String,
    ty: FieldTy,
    cardinality: Cardinality,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum FieldTy {
    Node(String),
    Token(String),
    /// Special token class which matches `SyntaxKind::is_identifier_like()`.
    Ident,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NodeDef {
    name: String,
    fields: Vec<Field>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EnumDef {
    name: String,
    variants: Vec<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Grammar {
    nodes: Vec<NodeDef>,
    enums: Vec<EnumDef>,
}

pub fn generate_ast(grammar_path: &Path) -> Result<String> {
    let grammar_src = std::fs::read_to_string(grammar_path)
        .with_context(|| format!("failed to read grammar `{}`", grammar_path.display()))?;
    let grammar = parse_grammar(&grammar_src)
        .with_context(|| format!("failed to parse `{}`", grammar_path.display()))?;
    rustfmt(render_ast(&grammar))
}

fn rustfmt(source: String) -> Result<String> {
    let mut child = Command::new("rustfmt")
        .args(["--emit", "stdout", "--edition", "2021"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn rustfmt")?;

    child
        .stdin
        .as_mut()
        .context("failed to open rustfmt stdin")?
        .write_all(source.as_bytes())
        .context("failed to write rustfmt stdin")?;

    let output = child
        .wait_with_output()
        .context("failed to read rustfmt output")?;
    if !output.status.success() {
        return Err(anyhow!(
            "rustfmt failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    String::from_utf8(output.stdout).context("rustfmt produced non-utf8 output")
}

fn strip_comment(line: &str) -> &str {
    let mut cut = line.len();
    if let Some(idx) = line.find('#') {
        cut = cut.min(idx);
    }
    if let Some(idx) = line.find("//") {
        cut = cut.min(idx);
    }
    &line[..cut]
}

fn is_indented(line: &str) -> bool {
    line.chars().next().is_some_and(|c| c.is_whitespace())
}

fn parse_grammar(src: &str) -> Result<Grammar> {
    let lines: Vec<&str> = src.lines().collect();
    let mut idx = 0usize;
    let mut grammar = Grammar::default();

    while idx < lines.len() {
        let raw = lines[idx];
        let line = strip_comment(raw);
        if line.trim().is_empty() {
            idx += 1;
            continue;
        }

        if is_indented(line) {
            return Err(anyhow!(
                "unexpected indentation at line {}: `{}`",
                idx + 1,
                raw.trim_end()
            ));
        }

        if let Some(open_idx) = line.find('{') {
            // Node definition.
            let name = line[..open_idx].trim();
            let after = line[open_idx + 1..].trim();
            if name.is_empty() {
                return Err(anyhow!("missing node name at line {}", idx + 1));
            }

            // `Foo {}` on a single line.
            if let Some(after) = after.strip_prefix('}') {
                let trailing = after.trim();
                if !trailing.is_empty() {
                    return Err(anyhow!(
                        "unexpected trailing tokens after `}}` at line {}",
                        idx + 1
                    ));
                }
                grammar.nodes.push(NodeDef {
                    name: name.to_string(),
                    fields: Vec::new(),
                });
                idx += 1;
                continue;
            }

            if !after.is_empty() {
                return Err(anyhow!(
                    "unexpected trailing tokens after `{{` at line {}",
                    idx + 1
                ));
            }

            idx += 1;
            let mut fields = Vec::new();
            while idx < lines.len() {
                let raw_field = lines[idx];
                let field_line = strip_comment(raw_field);
                if field_line.trim().is_empty() {
                    idx += 1;
                    continue;
                }

                if !is_indented(field_line) && field_line.trim() == "}" {
                    break;
                }

                let field_trimmed = field_line.trim();
                let (field_name, mut field_ty) = field_trimmed
                    .split_once(':')
                    .ok_or_else(|| anyhow!("expected `name: Type` at line {}", idx + 1))?;
                let field_name = field_name.trim();
                if field_name.is_empty() {
                    return Err(anyhow!("missing field name at line {}", idx + 1));
                }

                field_ty = field_ty.trim();
                if field_ty.ends_with(',') {
                    field_ty = field_ty[..field_ty.len() - 1].trim();
                }

                let (ty, cardinality) = if let Some(stripped) = field_ty.strip_suffix('?') {
                    (stripped.trim(), Cardinality::Optional)
                } else if let Some(stripped) = field_ty.strip_suffix('*') {
                    (stripped.trim(), Cardinality::Many)
                } else if let Some(stripped) = field_ty.strip_suffix('+') {
                    // Treat `+` like `*` for accessors; the parser can be lossy.
                    (stripped.trim(), Cardinality::Many)
                } else {
                    (field_ty, Cardinality::One)
                };

                if ty.is_empty() {
                    return Err(anyhow!("missing field type at line {}", idx + 1));
                }

                fields.push(Field {
                    name: field_name.to_string(),
                    ty: parse_field_ty(ty)
                        .with_context(|| format!("invalid field type at line {}", idx + 1))?,
                    cardinality,
                });
                idx += 1;
            }

            if idx >= lines.len() {
                return Err(anyhow!(
                    "unterminated node definition `{name}` (missing `}}`)"
                ));
            }

            // Consume `}`.
            idx += 1;

            grammar.nodes.push(NodeDef {
                name: name.to_string(),
                fields,
            });
            continue;
        }

        if let Some(eq_idx) = line.find('=') {
            // Enum (union) definition. Continuation lines must be indented.
            let name = line[..eq_idx].trim();
            let mut rhs = line[eq_idx + 1..].trim().to_string();
            idx += 1;

            while idx < lines.len() {
                let raw_next = lines[idx];
                let next = strip_comment(raw_next);
                if next.trim().is_empty() {
                    idx += 1;
                    continue;
                }
                if !is_indented(next) {
                    break;
                }
                if !rhs.is_empty() {
                    rhs.push(' ');
                }
                rhs.push_str(next.trim());
                idx += 1;
            }

            let variants: Vec<String> = rhs
                .split('|')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .collect();

            if name.is_empty() {
                return Err(anyhow!("missing enum name"));
            }
            if variants.is_empty() {
                return Err(anyhow!("enum `{name}` has no variants (line {})", idx + 1));
            }

            grammar.enums.push(EnumDef {
                name: name.to_string(),
                variants,
            });
            continue;
        }

        return Err(anyhow!(
            "expected a node (`Foo {{ ... }}`) or enum (`Bar = A | B`) at line {}",
            idx + 1
        ));
    }

    Ok(grammar)
}

fn parse_field_ty(src: &str) -> Result<FieldTy> {
    if src == "Ident" {
        return Ok(FieldTy::Ident);
    }

    if let Some(rest) = src.strip_prefix("Token(") {
        let Some(inner) = rest.strip_suffix(')') else {
            return Err(anyhow!("unterminated token type `{src}` (missing `)`)"));
        };
        let kind = inner.trim();
        if kind.is_empty() {
            return Err(anyhow!("missing token kind in `{src}`"));
        }
        if !kind.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(anyhow!("invalid token kind `{kind}` in `{src}`"));
        }
        return Ok(FieldTy::Token(kind.to_string()));
    }

    Ok(FieldTy::Node(src.to_string()))
}

fn render_ast(grammar: &Grammar) -> String {
    let mut out = String::new();
    out.push_str("//! Generated file, do not edit by hand.\n");
    out.push_str("//!\n");
    out.push_str("//! To regenerate, run:\n");
    out.push_str("//!   cargo xtask codegen\n\n");

    out.push_str("use crate::ast::{support, AstNode};\n");
    out.push_str("use crate::parser::{SyntaxNode, SyntaxToken};\n");
    out.push_str("use crate::syntax_kind::SyntaxKind;\n\n");

    // First generate structs for concrete nodes.
    for node in &grammar.nodes {
        render_node(&mut out, node);
        out.push('\n');
    }

    // Then generate enums (typed unions).
    for enm in &grammar.enums {
        render_enum(&mut out, grammar, enm);
        out.push('\n');
    }

    out
}

fn render_node(out: &mut String, node: &NodeDef) {
    use std::fmt::Write;

    let _ = writeln!(out, "#[derive(Debug, Clone, PartialEq, Eq)]");
    let _ = writeln!(out, "pub struct {} {{", node.name);
    let _ = writeln!(out, "    syntax: SyntaxNode,");
    let _ = writeln!(out, "}}");
    let _ = writeln!(out);

    let _ = writeln!(out, "impl AstNode for {} {{", node.name);
    let _ = writeln!(out, "    fn can_cast(kind: SyntaxKind) -> bool {{");
    let _ = writeln!(out, "        kind == SyntaxKind::{}", node.name);
    let _ = writeln!(out, "    }}");
    let _ = writeln!(out);
    let _ = writeln!(out, "    fn cast(syntax: SyntaxNode) -> Option<Self> {{");
    let _ = writeln!(
        out,
        "        Self::can_cast(syntax.kind()).then_some(Self {{ syntax }})"
    );
    let _ = writeln!(out, "    }}");
    let _ = writeln!(out);
    let _ = writeln!(out, "    fn syntax(&self) -> &SyntaxNode {{");
    let _ = writeln!(out, "        &self.syntax");
    let _ = writeln!(out, "    }}");
    let _ = writeln!(out, "}}");

    if node.fields.is_empty() {
        return;
    }

    let _ = writeln!(out);
    let _ = writeln!(out, "impl {} {{", node.name);

    // Track duplicate field types so we can select nth occurrence.
    let mut seen_counts: std::collections::HashMap<FieldTy, usize> =
        std::collections::HashMap::new();
    for field in &node.fields {
        let count = seen_counts.entry(field.ty.clone()).or_insert(0);
        let nth = *count;
        *count += 1;

        render_field_accessor(out, field, nth);
        let _ = writeln!(out);
    }

    let _ = writeln!(out, "}}");
}

fn render_field_accessor(out: &mut String, field: &Field, nth: usize) {
    use std::fmt::Write;

    let name = &field.name;

    match (&field.ty, field.cardinality) {
        (FieldTy::Ident, Cardinality::Many) => {
            let _ = writeln!(
                out,
                "    pub fn {name}(&self) -> impl Iterator<Item = SyntaxToken> + '_ {{"
            );
            let _ = writeln!(out, "        support::ident_tokens(&self.syntax)");
            let _ = writeln!(out, "    }}");
        }
        (FieldTy::Ident, Cardinality::One | Cardinality::Optional) => {
            if nth == 0 {
                let _ = writeln!(out, "    pub fn {name}(&self) -> Option<SyntaxToken> {{");
                let _ = writeln!(out, "        support::ident_token(&self.syntax)");
                let _ = writeln!(out, "    }}");
            } else {
                let _ = writeln!(out, "    pub fn {name}(&self) -> Option<SyntaxToken> {{");
                let _ = writeln!(
                    out,
                    "        support::ident_tokens(&self.syntax).nth({nth})"
                );
                let _ = writeln!(out, "    }}");
            }
        }
        (FieldTy::Token(kind), Cardinality::Many) => {
            let _ = writeln!(
                out,
                "    pub fn {name}(&self) -> impl Iterator<Item = SyntaxToken> + '_ {{"
            );
            let _ = writeln!(
                out,
                "        support::tokens(&self.syntax, SyntaxKind::{kind})"
            );
            let _ = writeln!(out, "    }}");
        }
        (FieldTy::Token(kind), Cardinality::One | Cardinality::Optional) => {
            let _ = writeln!(out, "    pub fn {name}(&self) -> Option<SyntaxToken> {{");
            if nth == 0 {
                let _ = writeln!(
                    out,
                    "        support::token(&self.syntax, SyntaxKind::{kind})"
                );
            } else {
                let _ = writeln!(
                    out,
                    "        support::tokens(&self.syntax, SyntaxKind::{kind}).nth({nth})"
                );
            }
            let _ = writeln!(out, "    }}");
        }
        (FieldTy::Node(ty), Cardinality::Many) => {
            let _ = writeln!(
                out,
                "    pub fn {name}(&self) -> impl Iterator<Item = {ty}> + '_ {{"
            );
            let _ = writeln!(out, "        support::children::<{ty}>(&self.syntax)");
            let _ = writeln!(out, "    }}");
        }
        (FieldTy::Node(ty), Cardinality::One | Cardinality::Optional) => {
            let _ = writeln!(out, "    pub fn {name}(&self) -> Option<{ty}> {{");
            if nth == 0 {
                let _ = writeln!(out, "        support::child::<{ty}>(&self.syntax)");
            } else {
                let _ = writeln!(
                    out,
                    "        support::children::<{ty}>(&self.syntax).nth({nth})"
                );
            }
            let _ = writeln!(out, "    }}");
        }
    }
}

fn render_enum(out: &mut String, grammar: &Grammar, enm: &EnumDef) {
    use std::fmt::Write;

    let _ = writeln!(out, "#[derive(Debug, Clone, PartialEq, Eq)]");
    let _ = writeln!(out, "#[non_exhaustive]");
    let _ = writeln!(out, "pub enum {} {{", enm.name);
    for var in &enm.variants {
        let _ = writeln!(out, "    {var}({var}),");
    }
    let _ = writeln!(out, "}}");
    let _ = writeln!(out);

    let _ = writeln!(out, "impl AstNode for {} {{", enm.name);
    let _ = writeln!(out, "    fn can_cast(kind: SyntaxKind) -> bool {{");
    for (i, var) in enm.variants.iter().enumerate() {
        let prefix = if i == 0 {
            "        "
        } else {
            "            || "
        };
        let _ = writeln!(out, "{prefix}{var}::can_cast(kind)");
    }
    let _ = writeln!(out, "    }}");
    let _ = writeln!(out);

    let _ = writeln!(out, "    fn cast(syntax: SyntaxNode) -> Option<Self> {{");
    let _ = writeln!(out, "        let kind = syntax.kind();");
    let _ = writeln!(out, "        if !Self::can_cast(kind) {{");
    let _ = writeln!(out, "            return None;");
    let _ = writeln!(out, "        }}");
    let _ = writeln!(out);

    for (i, var) in enm.variants.iter().enumerate() {
        let expr = format!("{}::cast(syntax.clone())", var);
        let _ = writeln!(
            out,
            "        if let Some(it) = {expr} {{ return Some(Self::{var}(it)); }}"
        );
        if i == enm.variants.len() - 1 {
            let _ = writeln!(out);
        }
    }

    let _ = writeln!(out, "        None");
    let _ = writeln!(out, "    }}");
    let _ = writeln!(out);

    let _ = writeln!(out, "    fn syntax(&self) -> &SyntaxNode {{");
    let _ = writeln!(out, "        match self {{");
    for var in &enm.variants {
        let _ = writeln!(out, "            Self::{var}(it) => it.syntax(),");
    }
    let _ = writeln!(out, "        }}");
    let _ = writeln!(out, "    }}");
    let _ = writeln!(out, "}}");

    // Sanity check: ensure enum variants are defined somewhere in the grammar.
    // This provides a better error message than the raw Rust compiler errors.
    let defined_types: std::collections::HashSet<&str> = grammar
        .nodes
        .iter()
        .map(|n| n.name.as_str())
        .chain(grammar.enums.iter().map(|e| e.name.as_str()))
        .collect();
    for var in &enm.variants {
        if !defined_types.contains(var.as_str()) && var != "Ident" {
            // Don't fail codegen (we want this to be usable while editing),
            // but keep this check for future extension.
            let _ = writeln!(
                out,
                "// NOTE: `{}` references unknown type `{}` in the grammar.",
                enm.name, var
            );
        }
    }
}

#[derive(Debug, Default)]
pub struct SyntaxLintReport {
    sources_scanned: usize,
    grammar_node_count: usize,
    grammar_enum_count: usize,
    emitted_node_count: usize,
    missing_wrappers: Vec<(String, String)>,
    unknown_node_kinds: Vec<String>,
    unknown_type_references: Vec<String>,
}

impl SyntaxLintReport {
    pub fn is_clean(&self) -> bool {
        self.missing_wrappers.is_empty()
            && self.unknown_node_kinds.is_empty()
            && self.unknown_type_references.is_empty()
    }
}

impl std::fmt::Display for SyntaxLintReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "syntax-lint report")?;
        writeln!(f, "  sources scanned: {}", self.sources_scanned)?;
        writeln!(
            f,
            "  grammar: {} nodes, {} enums",
            self.grammar_node_count, self.grammar_enum_count
        )?;
        writeln!(
            f,
            "  parser emitted: {} node kinds",
            self.emitted_node_count
        )?;

        if self.is_clean() {
            writeln!(f)?;
            writeln!(f, "OK: no drift detected")?;
            return Ok(());
        }

        if !self.unknown_type_references.is_empty() {
            writeln!(f)?;
            writeln!(f, "Grammar references unknown types:")?;
            for ty in &self.unknown_type_references {
                writeln!(f, "  - {ty}")?;
            }
        }

        if !self.unknown_node_kinds.is_empty() {
            writeln!(f)?;
            writeln!(f, "Grammar defines nodes missing from SyntaxKind:")?;
            for kind in &self.unknown_node_kinds {
                writeln!(f, "  - {kind}")?;
            }
        }

        if !self.missing_wrappers.is_empty() {
            writeln!(f)?;
            writeln!(
                f,
                "Parser emitted node kinds without typed wrappers in `grammar/java.syntax`:"
            )?;
            for (kind, sample) in &self.missing_wrappers {
                writeln!(f, "  - {kind} (e.g. {sample})")?;
            }
        }

        writeln!(f)?;
        writeln!(
            f,
            "Fix: update `crates/nova-syntax/grammar/java.syntax`, then run:"
        )?;
        writeln!(f, "  cargo xtask codegen")?;
        Ok(())
    }
}

pub fn syntax_lint_report(repo_root: &Path) -> Result<SyntaxLintReport> {
    let syntax_kind_path = repo_root.join("crates/nova-syntax/src/syntax_kind.rs");
    let grammar_path = repo_root.join("crates/nova-syntax/grammar/java.syntax");

    let (syntax_kind_all_kinds, syntax_kind_node_kinds) =
        parse_syntax_kind_kinds(&syntax_kind_path)
            .with_context(|| format!("failed to parse `{}`", syntax_kind_path.display()))?;

    let grammar_src = std::fs::read_to_string(&grammar_path)
        .with_context(|| format!("failed to read `{}`", grammar_path.display()))?;
    let grammar = parse_grammar(&grammar_src)
        .with_context(|| format!("failed to parse `{}`", grammar_path.display()))?;

    let grammar_node_names: std::collections::BTreeSet<String> =
        grammar.nodes.iter().map(|n| n.name.clone()).collect();
    let grammar_enum_names: std::collections::BTreeSet<String> =
        grammar.enums.iter().map(|e| e.name.clone()).collect();

    let defined_types: std::collections::BTreeSet<String> = grammar_node_names
        .iter()
        .cloned()
        .chain(grammar_enum_names.iter().cloned())
        .collect();

    let mut unknown_type_references = Vec::new();
    for node in &grammar.nodes {
        for field in &node.fields {
            match &field.ty {
                FieldTy::Node(ty) => {
                    if !defined_types.contains(ty) {
                        unknown_type_references.push(format!("{}.{}: {ty}", node.name, field.name));
                    }
                }
                FieldTy::Token(kind) => {
                    if !syntax_kind_all_kinds.contains(kind) {
                        unknown_type_references
                            .push(format!("{}.{}: Token({kind})", node.name, field.name));
                    }
                }
                FieldTy::Ident => {}
            }
        }
    }
    for enm in &grammar.enums {
        for variant in &enm.variants {
            if !defined_types.contains(variant) {
                unknown_type_references.push(format!("{} = ... | {}", enm.name, variant));
            }
        }
    }

    let mut unknown_node_kinds = Vec::new();
    for node in &grammar.nodes {
        if !syntax_kind_node_kinds.contains(&node.name) {
            unknown_node_kinds.push(node.name.clone());
        }
    }

    let mut java_files = Vec::new();
    collect_java_files(
        &repo_root.join("crates/nova-syntax/testdata/parser"),
        &mut java_files,
    )?;
    collect_java_files(
        &repo_root.join("crates/nova-syntax/testdata/javac/ok"),
        &mut java_files,
    )?;
    collect_java_files(
        &repo_root.join("crates/nova-syntax/testdata/javac/err"),
        &mut java_files,
    )?;
    collect_java_files(
        &repo_root.join("crates/nova-syntax/testdata/recovery"),
        &mut java_files,
    )?;
    java_files.sort();
    java_files.dedup();

    let mut sources: Vec<SyntaxLintSource> =
        java_files.into_iter().map(SyntaxLintSource::Path).collect();

    // Small "smoke test" sources that exercise nodes not currently covered by the fixture corpus.
    sources.push(SyntaxLintSource::Inline {
        name: "local_type_declaration_statement".to_string(),
        text: "class Foo { void m() { class Local {} } }".to_string(),
    });
    // Fragment entry points are used by incremental reparsing and debugger evaluation; they emit
    // additional root node kinds that aren't covered by parsing full compilation units.
    sources.push(SyntaxLintSource::ExpressionRoot {
        name: "expression_root".to_string(),
        text: "a + b".to_string(),
    });
    sources.push(SyntaxLintSource::ExpressionFragment {
        name: "expression_fragment".to_string(),
        text: "a + b".to_string(),
    });
    sources.push(SyntaxLintSource::StatementFragment {
        name: "statement_fragment".to_string(),
        text: "return 1;".to_string(),
    });
    sources.push(SyntaxLintSource::BlockFragment {
        name: "block_fragment".to_string(),
        text: "{ int x = 1; }".to_string(),
    });
    sources.push(SyntaxLintSource::ClassMemberFragment {
        name: "class_member_fragment".to_string(),
        text: "int x = 1;".to_string(),
    });

    let (emitted_node_kinds, first_seen) = collect_emitted_node_kinds(repo_root, &sources)
        .context("failed to collect parser nodes")?;

    let mut missing_wrappers: Vec<(String, String)> = emitted_node_kinds
        .iter()
        .filter(|kind| !grammar_node_names.contains(*kind))
        .map(|kind| {
            let sample = first_seen
                .get(kind)
                .cloned()
                .unwrap_or_else(|| "<unknown source>".to_string());
            (kind.clone(), sample)
        })
        .collect();

    missing_wrappers.sort_by(|a, b| a.0.cmp(&b.0));
    unknown_node_kinds.sort();
    unknown_type_references.sort();

    Ok(SyntaxLintReport {
        sources_scanned: sources.len(),
        grammar_node_count: grammar_node_names.len(),
        grammar_enum_count: grammar_enum_names.len(),
        emitted_node_count: emitted_node_kinds.len(),
        missing_wrappers,
        unknown_node_kinds,
        unknown_type_references,
    })
}

#[derive(Debug, Clone)]
enum SyntaxLintSource {
    Path(PathBuf),
    Inline { name: String, text: String },
    ExpressionRoot { name: String, text: String },
    ExpressionFragment { name: String, text: String },
    StatementFragment { name: String, text: String },
    BlockFragment { name: String, text: String },
    ClassMemberFragment { name: String, text: String },
}

fn collect_java_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)
        .with_context(|| format!("failed to read directory `{}`", dir.display()))?
    {
        let entry = entry
            .with_context(|| format!("failed to read directory entry in `{}`", dir.display()))?;
        let path = entry.path();
        if path.is_dir() {
            collect_java_files(&path, out)?;
            continue;
        }
        if path
            .extension()
            .is_some_and(|ext| ext == std::ffi::OsStr::new("java"))
        {
            out.push(path);
        }
    }
    Ok(())
}

fn collect_emitted_node_kinds(
    repo_root: &Path,
    sources: &[SyntaxLintSource],
) -> Result<(
    std::collections::BTreeSet<String>,
    std::collections::BTreeMap<String, String>,
)> {
    let mut emitted: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut first_seen: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();

    for source in sources {
        let (name, parsed) = match source {
            SyntaxLintSource::Path(path) => {
                let rel = path
                    .strip_prefix(repo_root)
                    .unwrap_or(path.as_path())
                    .display()
                    .to_string();
                let text = std::fs::read_to_string(path)
                    .with_context(|| format!("failed to read `{}`", path.display()))?;
                (rel, nova_syntax::parse_java(&text))
            }
            SyntaxLintSource::Inline { name, text } => {
                (format!("<inline:{name}>"), nova_syntax::parse_java(text))
            }
            SyntaxLintSource::ExpressionRoot { name, text } => (
                format!("<expr-root:{name}>"),
                nova_syntax::parse_java_expression(text),
            ),
            SyntaxLintSource::ExpressionFragment { name, text } => (
                format!("<expr-fragment:{name}>"),
                nova_syntax::parse_java_expression_fragment(text, 0).parse,
            ),
            SyntaxLintSource::StatementFragment { name, text } => (
                format!("<stmt-fragment:{name}>"),
                nova_syntax::parse_java_statement_fragment(text, 0).parse,
            ),
            SyntaxLintSource::BlockFragment { name, text } => (
                format!("<block-fragment:{name}>"),
                nova_syntax::parse_java_block_fragment(text, 0).parse,
            ),
            SyntaxLintSource::ClassMemberFragment { name, text } => (
                format!("<member-fragment:{name}>"),
                nova_syntax::parse_java_class_member_fragment(text, 0).parse,
            ),
        };
        let root = parsed.syntax();

        record_emitted_kind(root.kind(), &name, &mut emitted, &mut first_seen);
        for node in root.descendants() {
            record_emitted_kind(node.kind(), &name, &mut emitted, &mut first_seen);
        }
    }

    Ok((emitted, first_seen))
}

fn record_emitted_kind(
    kind: nova_syntax::SyntaxKind,
    source: &str,
    emitted: &mut std::collections::BTreeSet<String>,
    first_seen: &mut std::collections::BTreeMap<String, String>,
) {
    // Error nodes are internal recovery artifacts; they don't need typed wrappers.
    if kind == nova_syntax::SyntaxKind::Error {
        return;
    }
    let name = format!("{kind:?}");
    emitted.insert(name.clone());
    first_seen.entry(name).or_insert_with(|| source.to_string());
}

fn parse_syntax_kind_kinds(
    path: &Path,
) -> Result<(
    std::collections::BTreeSet<String>,
    std::collections::BTreeSet<String>,
)> {
    let variants = parse_syntax_kind_variants(path)?;
    let all = variants.iter().cloned().collect();
    let nodes = syntax_kind_node_kinds_from_variants(&variants)?;
    Ok((all, nodes))
}

fn parse_syntax_kind_variants(path: &Path) -> Result<Vec<String>> {
    let src = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read `{}`", path.display()))?;

    let mut variants = Vec::new();
    let mut in_enum = false;
    for raw in src.lines() {
        let line = strip_comment(raw).trim();
        if !in_enum {
            if line.contains("enum SyntaxKind") && line.contains('{') {
                in_enum = true;
            }
            continue;
        }

        if line.starts_with('}') {
            break;
        }

        if line.is_empty() || line.starts_with('#') || line.starts_with("///") {
            continue;
        }

        let Some(name) = parse_rust_ident(line) else {
            continue;
        };
        variants.push(name);
    }

    Ok(variants)
}

fn syntax_kind_node_kinds_from_variants(
    variants: &[String],
) -> Result<std::collections::BTreeSet<String>> {
    let start = variants
        .iter()
        .position(|v| v == "CompilationUnit")
        .ok_or_else(|| anyhow!("failed to find `CompilationUnit` variant in SyntaxKind"))?;
    let end = variants
        .iter()
        .position(|v| v == "__Last")
        .ok_or_else(|| anyhow!("failed to find `__Last` variant in SyntaxKind"))?;

    let deny: std::collections::HashSet<&str> = [
        "MissingSemicolon",
        "MissingRParen",
        "MissingRBrace",
        "MissingRBracket",
        "MissingGreater",
    ]
    .into_iter()
    .collect();

    let mut nodes: std::collections::BTreeSet<String> = variants[start..end]
        .iter()
        .filter(|name| !deny.contains(name.as_str()))
        .cloned()
        .collect();

    // `SyntaxKind::Error` lives in the token section but is also used as a node kind for
    // error-recovery. The grammar includes an `Error {}` wrapper, so treat it as a valid node.
    if variants.iter().any(|v| v == "Error") {
        nodes.insert("Error".to_string());
    }

    Ok(nodes)
}

fn parse_rust_ident(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    let mut chars = trimmed.chars();
    let first = chars.next()?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return None;
    }

    let mut name = String::new();
    name.push(first);
    for c in chars {
        if c.is_ascii_alphanumeric() || c == '_' {
            name.push(c);
        } else {
            break;
        }
    }

    let rest = trimmed[name.len()..].trim_start();
    // Enum variants are written as `Foo,` in `syntax_kind.rs`.
    if rest.starts_with(',') {
        Some(name)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_token_field_syntax() {
        let src = r#"
Foo {
  arrow_token: Token(Arrow)?
  semicolons: Token(Semicolon)*
  name_token: Ident?
  child: Bar?
}
Bar {}
"#;

        let grammar = parse_grammar(src).expect("parse grammar");
        assert_eq!(grammar.nodes.len(), 2);

        let foo = &grammar.nodes[0];
        assert_eq!(foo.name, "Foo");
        assert_eq!(foo.fields.len(), 4);

        assert_eq!(foo.fields[0].name, "arrow_token");
        assert_eq!(foo.fields[0].ty, FieldTy::Token("Arrow".to_string()));
        assert_eq!(foo.fields[0].cardinality, Cardinality::Optional);

        assert_eq!(foo.fields[1].name, "semicolons");
        assert_eq!(foo.fields[1].ty, FieldTy::Token("Semicolon".to_string()));
        assert_eq!(foo.fields[1].cardinality, Cardinality::Many);

        assert_eq!(foo.fields[2].name, "name_token");
        assert_eq!(foo.fields[2].ty, FieldTy::Ident);
        assert_eq!(foo.fields[2].cardinality, Cardinality::Optional);

        assert_eq!(foo.fields[3].name, "child");
        assert_eq!(foo.fields[3].ty, FieldTy::Node("Bar".to_string()));
        assert_eq!(foo.fields[3].cardinality, Cardinality::Optional);
    }

    #[test]
    fn renders_token_accessors() {
        let src = r#"
Foo {
  arrow_token: Token(Arrow)?
  semicolons: Token(Semicolon)*
  name_token: Ident?
  child: Bar?
  semi1: Token(Semicolon)?
  semi2: Token(Semicolon)?
}
Bar {}
"#;

        let grammar = parse_grammar(src).expect("parse grammar");
        let code = render_ast(&grammar);

        assert!(code.contains("pub fn arrow_token(&self) -> Option<SyntaxToken>"));
        assert!(code.contains("support::token(&self.syntax, SyntaxKind::Arrow)"));

        assert!(code.contains("pub fn semicolons(&self) -> impl Iterator<Item = SyntaxToken> + '_"));
        assert!(code.contains("support::tokens(&self.syntax, SyntaxKind::Semicolon)"));

        assert!(code.contains("pub fn name_token(&self) -> Option<SyntaxToken>"));
        assert!(code.contains("support::ident_token(&self.syntax)"));

        assert!(code.contains("support::tokens(&self.syntax, SyntaxKind::Semicolon).nth(1)"));
    }
}
