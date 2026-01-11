//! A small Prettier/Wadler-Leijen style document model and pretty printer.
//!
//! This module is intentionally self-contained: it does not depend on the existing token-based
//! Java formatter. It will serve as the foundation for an upcoming AST-aware Java formatter.
//!
//! The core ideas:
//! - [`Doc`] is a composable document tree (text, concatenation, line breaks, groups, indentation).
//! - [`print`] renders a [`Doc`] to a `String` using a deterministic algorithm inspired by
//!   Prettier's printer.
//! - [`Group`](Doc::group) tries to render its contents in [`Mode::Flat`] (no line breaks), but
//!   falls back to [`Mode::Break`] when it doesn't fit within `max_width`.
//!
//! This module deliberately avoids heavyweight Unicode dependencies. Width is approximated using
//! `text.len()` for ASCII and `text.chars().count()` otherwise.

use std::borrow::Cow;
use std::rc::Rc;

/// Rendering configuration for [`print`].
#[derive(Debug, Clone, Copy)]
pub struct PrintConfig {
    pub max_width: usize,
    pub indent_width: usize,
}

impl Default for PrintConfig {
    fn default() -> Self {
        Self {
            max_width: 100,
            indent_width: 4,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Flat,
    Break,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// A regular line break: `" "` in flat mode, `"\n"` in break mode.
    Line,
    /// A soft line break: `""` in flat mode, `"\n"` in break mode.
    Soft,
    /// A hard line break: always `"\n"`, and forces any containing group to break.
    Hard,
}

#[derive(Debug)]
enum DocKind<'a> {
    Nil,
    Text(Cow<'a, str>),
    Concat(Vec<Doc<'a>>),
    Group(Doc<'a>),
    Nest(usize, Doc<'a>),
    /// Increase indentation by `PrintConfig::indent_width`.
    Indent(Doc<'a>),
    Line(LineKind),
    IfBreak {
        break_doc: Doc<'a>,
        flat_doc: Doc<'a>,
    },
}

/// A composable pretty-printing document.
///
/// `Doc` is cheaply cloneable (internally reference counted) so the printer can perform `fits()`
/// lookahead without copying the full tree.
#[derive(Clone, Debug)]
pub struct Doc<'a>(Rc<DocKind<'a>>);

impl<'a> Doc<'a> {
    fn new(kind: DocKind<'a>) -> Self {
        Self(Rc::new(kind))
    }

    fn kind(&self) -> &DocKind<'a> {
        self.0.as_ref()
    }

    /// An empty document.
    pub fn nil() -> Self {
        Self::new(DocKind::Nil)
    }

    /// A text fragment.
    pub fn text<T>(text: T) -> Self
    where
        T: Into<Cow<'a, str>>,
    {
        Self::new(DocKind::Text(text.into()))
    }

    /// Concatenate documents in order.
    ///
    /// Empty docs are discarded and nested concatenations are flattened.
    pub fn concat<I>(docs: I) -> Self
    where
        I: IntoIterator<Item = Doc<'a>>,
    {
        let mut parts = Vec::new();
        for doc in docs {
            match doc.kind() {
                DocKind::Nil => {}
                DocKind::Concat(inner) => parts.extend(inner.iter().cloned()),
                _ => parts.push(doc),
            }
        }

        match parts.len() {
            0 => Self::nil(),
            1 => parts.pop().unwrap(),
            _ => Self::new(DocKind::Concat(parts)),
        }
    }

    /// Wrap the document in a [`Group`](DocKind::Group).
    pub fn group(self) -> Self {
        Self::new(DocKind::Group(self))
    }

    /// Increase indentation by `spaces` for contained line breaks.
    pub fn nest(self, spaces: usize) -> Self {
        Self::new(DocKind::Nest(spaces, self))
    }

    /// Increase indentation by `config.indent_width` for contained line breaks.
    pub fn indent(self) -> Self {
        Self::new(DocKind::Indent(self))
    }

    /// A line break that becomes `" "` in flat mode.
    pub fn line() -> Self {
        Self::new(DocKind::Line(LineKind::Line))
    }

    /// A line break that becomes `""` in flat mode.
    pub fn softline() -> Self {
        Self::new(DocKind::Line(LineKind::Soft))
    }

    /// A line break that is always rendered as `"\n"`.
    pub fn hardline() -> Self {
        Self::new(DocKind::Line(LineKind::Hard))
    }

    /// Choose between `break_doc` and `flat_doc` based on the rendering mode of the current group.
    ///
    /// - [`Mode::Flat`] => `flat_doc`
    /// - [`Mode::Break`] => `break_doc`
    pub fn if_break(break_doc: Doc<'a>, flat_doc: Doc<'a>) -> Self {
        Self::new(DocKind::IfBreak {
            break_doc,
            flat_doc,
        })
    }
}

#[derive(Clone, Debug)]
struct Command<'a> {
    indent: usize,
    mode: Mode,
    doc: Doc<'a>,
}

/// Render `doc` to a `String`.
///
/// The algorithm is iterative:
/// - Maintain a stack of [`Command`]s.
/// - When encountering a group in [`Mode::Break`], use [`fits`] lookahead to decide whether the
///   group can be rendered in [`Mode::Flat`] within the remaining line width.
#[must_use]
pub fn print<'a>(doc: Doc<'a>, config: PrintConfig) -> String {
    let mut out = String::new();
    let mut pos: usize = 0;

    let mut stack = vec![Command {
        indent: 0,
        mode: Mode::Break,
        doc,
    }];

    while let Some(Command { indent, mode, doc }) = stack.pop() {
        match doc.kind() {
            DocKind::Nil => {}
            DocKind::Text(text) => {
                out.push_str(text);
                pos = pos.saturating_add(text_width(text));
            }
            DocKind::Concat(parts) => {
                for part in parts.iter().rev() {
                    stack.push(Command {
                        indent,
                        mode,
                        doc: part.clone(),
                    });
                }
            }
            DocKind::Group(inner) => match mode {
                Mode::Flat => stack.push(Command {
                    indent,
                    mode: Mode::Flat,
                    doc: inner.clone(),
                }),
                Mode::Break => {
                    let remaining_width = config.max_width as isize - pos as isize;
                    let mut lookahead = stack.clone();
                    lookahead.push(Command {
                        indent,
                        mode: Mode::Flat,
                        doc: inner.clone(),
                    });

                    let next_mode = if fits(remaining_width, &lookahead, config) {
                        Mode::Flat
                    } else {
                        Mode::Break
                    };
                    stack.push(Command {
                        indent,
                        mode: next_mode,
                        doc: inner.clone(),
                    });
                }
            },
            DocKind::Nest(spaces, inner) => stack.push(Command {
                indent: indent.saturating_add(*spaces),
                mode,
                doc: inner.clone(),
            }),
            DocKind::Indent(inner) => stack.push(Command {
                indent: indent.saturating_add(config.indent_width),
                mode,
                doc: inner.clone(),
            }),
            DocKind::Line(kind) => match mode {
                Mode::Flat => match kind {
                    LineKind::Line => {
                        out.push(' ');
                        pos = pos.saturating_add(1);
                    }
                    LineKind::Soft => {}
                    LineKind::Hard => {
                        out.push('\n');
                        push_spaces(&mut out, indent);
                        pos = indent;
                    }
                },
                Mode::Break => {
                    out.push('\n');
                    push_spaces(&mut out, indent);
                    pos = indent;
                }
            },
            DocKind::IfBreak {
                break_doc,
                flat_doc,
            } => {
                let chosen = if mode == Mode::Break {
                    break_doc.clone()
                } else {
                    flat_doc.clone()
                };
                stack.push(Command {
                    indent,
                    mode,
                    doc: chosen,
                });
            }
        }
    }

    out
}

fn push_spaces(out: &mut String, count: usize) {
    out.extend(std::iter::repeat(' ').take(count));
}

fn text_width(text: &str) -> usize {
    if text.is_ascii() {
        text.len()
    } else {
        text.chars().count()
    }
}

fn fits<'a>(mut remaining_width: isize, cmds: &[Command<'a>], config: PrintConfig) -> bool {
    if remaining_width < 0 {
        return false;
    }

    let mut stack: Vec<Command<'a>> = cmds.to_vec();

    while remaining_width >= 0 {
        let Some(Command { indent, mode, doc }) = stack.pop() else {
            return true;
        };

        match doc.kind() {
            DocKind::Nil => {}
            DocKind::Text(text) => remaining_width -= text_width(text) as isize,
            DocKind::Concat(parts) => {
                for part in parts.iter().rev() {
                    stack.push(Command {
                        indent,
                        mode,
                        doc: part.clone(),
                    });
                }
            }
            DocKind::Group(inner) => stack.push(Command {
                indent,
                mode,
                doc: inner.clone(),
            }),
            DocKind::Nest(spaces, inner) => stack.push(Command {
                indent: indent.saturating_add(*spaces),
                mode,
                doc: inner.clone(),
            }),
            DocKind::Indent(inner) => stack.push(Command {
                indent: indent.saturating_add(config.indent_width),
                mode,
                doc: inner.clone(),
            }),
            DocKind::Line(kind) => match mode {
                Mode::Flat => match kind {
                    LineKind::Line => remaining_width -= 1,
                    LineKind::Soft => {}
                    LineKind::Hard => return false,
                },
                Mode::Break => return true,
            },
            DocKind::IfBreak {
                break_doc,
                flat_doc,
            } => {
                let chosen = if mode == Mode::Break {
                    break_doc.clone()
                } else {
                    flat_doc.clone()
                };
                stack.push(Command {
                    indent,
                    mode,
                    doc: chosen,
                });
            }
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn cfg(max_width: usize) -> PrintConfig {
        PrintConfig {
            max_width,
            indent_width: 4,
        }
    }

    #[test]
    fn group_selects_flat_or_break() {
        let doc = Doc::concat([Doc::text("a"), Doc::line(), Doc::text("b")]).group();

        assert_eq!(print(doc.clone(), cfg(10)), "a b");
        assert_eq!(print(doc, cfg(1)), "a\nb");
    }

    #[test]
    fn fits_considers_trailing_stack() {
        let doc = Doc::concat([
            Doc::concat([Doc::text("a"), Doc::line(), Doc::text("b")]).group(),
            Doc::text("c"),
        ]);

        // The group itself would fit as `a b` (len=3), but it must consider the trailing `c`
        // (len=1) when deciding between flat vs break.
        assert_eq!(print(doc, cfg(3)), "a\nbc");
    }

    #[test]
    fn indent_only_applies_after_break() {
        let doc = Doc::concat([
            Doc::text("a"),
            Doc::concat([Doc::line(), Doc::text("b")]).indent(),
        ])
        .group();

        assert_eq!(print(doc.clone(), cfg(10)), "a b");
        assert_eq!(print(doc, cfg(1)), "a\n    b");
    }

    #[test]
    fn softline_vs_line_in_flat_mode() {
        let line_doc = Doc::concat([Doc::text("a"), Doc::line(), Doc::text("b")]).group();
        let softline_doc = Doc::concat([Doc::text("a"), Doc::softline(), Doc::text("b")]).group();

        assert_eq!(print(line_doc, cfg(10)), "a b");
        assert_eq!(print(softline_doc, cfg(10)), "ab");
    }

    #[test]
    fn hardline_forces_group_to_break() {
        let doc = Doc::concat([
            Doc::text("a"),
            Doc::if_break(Doc::text("!"), Doc::text("?")),
            Doc::hardline(),
            Doc::text("b"),
        ])
        .group();

        // Even though the content would otherwise fit, `hardline` prevents a group from being
        // printed in flat mode, so `IfBreak` must select the broken variant.
        assert_eq!(print(doc, cfg(100)), "a!\nb");
    }

    #[test]
    fn ifbreak_selects_variant() {
        let doc = Doc::concat([
            Doc::text("a"),
            Doc::if_break(Doc::text("X"), Doc::text("Y")),
            Doc::line(),
            Doc::text("b"),
        ])
        .group();

        assert_eq!(print(doc.clone(), cfg(10)), "aY b");
        assert_eq!(print(doc, cfg(2)), "aX\nb");
    }
}
