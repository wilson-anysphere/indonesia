use std::env;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

pub fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    let cmd = args
        .next()
        .ok_or_else(|| anyhow!("expected a command (try `codegen`)"))?;

    match cmd.as_str() {
        "codegen" => {
            if let Some(arg) = args.next() {
                return Err(anyhow!(
                    "unexpected argument `{arg}` (usage: cargo xtask codegen)"
                ));
            }
            codegen()?;
        }
        _ => {
            return Err(anyhow!("unknown command `{cmd}` (supported: `codegen`)"));
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

fn repo_root() -> Result<PathBuf> {
    // `cargo run -p xtask -- ...` is executed with the workspace root as CWD.
    // Keep this helper anyway so tests can call into codegen from arbitrary CWDs.
    let cwd = env::current_dir().context("failed to read current working directory")?;
    Ok(cwd)
}

fn write_if_changed(path: &Path, contents: &str) -> Result<()> {
    let existing = std::fs::read_to_string(path).ok();
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
    ty: String,
    cardinality: Cardinality,
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
    Ok(render_ast(&grammar))
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
            if after.starts_with('}') {
                let trailing = after[1..].trim();
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
                    ty: ty.to_string(),
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
    let mut seen_counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for field in &node.fields {
        let count = seen_counts.entry(&field.ty).or_insert(0);
        let nth = *count;
        *count += 1;

        render_field_accessor(out, field, nth);
        let _ = writeln!(out);
    }

    let _ = writeln!(out, "}}");
}

fn render_field_accessor(out: &mut String, field: &Field, nth: usize) {
    use std::fmt::Write;

    let ty = &field.ty;
    let name = &field.name;

    match (ty.as_str(), field.cardinality) {
        ("Ident", Cardinality::Many) => {
            let _ = writeln!(
                out,
                "    pub fn {name}(&self) -> impl Iterator<Item = SyntaxToken> + '_ {{"
            );
            let _ = writeln!(out, "        support::ident_tokens(&self.syntax)");
            let _ = writeln!(out, "    }}");
        }
        ("Ident", Cardinality::One | Cardinality::Optional) => {
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
        _ => match field.cardinality {
            Cardinality::Many => {
                let _ = writeln!(
                    out,
                    "    pub fn {name}(&self) -> impl Iterator<Item = {ty}> + '_ {{"
                );
                let _ = writeln!(out, "        support::children::<{ty}>(&self.syntax)");
                let _ = writeln!(out, "    }}");
            }
            Cardinality::One | Cardinality::Optional => {
                if nth == 0 {
                    let _ = writeln!(out, "    pub fn {name}(&self) -> Option<{ty}> {{");
                    let _ = writeln!(out, "        support::child::<{ty}>(&self.syntax)");
                    let _ = writeln!(out, "    }}");
                } else {
                    let _ = writeln!(out, "    pub fn {name}(&self) -> Option<{ty}> {{");
                    let _ = writeln!(
                        out,
                        "        support::children::<{ty}>(&self.syntax).nth({nth})"
                    );
                    let _ = writeln!(out, "    }}");
                }
            }
        },
    }
}

fn render_enum(out: &mut String, grammar: &Grammar, enm: &EnumDef) {
    use std::fmt::Write;

    let _ = writeln!(out, "#[derive(Debug, Clone, PartialEq, Eq)]");
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
