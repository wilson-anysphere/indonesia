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
    pub newline: &'static str,
}

impl Default for PrintConfig {
    fn default() -> Self {
        Self {
            max_width: 100,
            indent_width: 4,
            newline: "\n",
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
    /// A regular line break: `" "` in flat mode, `config.newline` in break mode.
    Line,
    /// A soft line break: `""` in flat mode, `config.newline` in break mode.
    Soft,
    /// A hard line break: always `config.newline`, and forces any containing group to break.
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
    /// Deferred content that is printed at the end of the current line.
    ///
    /// This is primarily used for trailing line comments (`// ...`) which must be rendered before
    /// the line break that ends the current line, even when surrounding groups decide to break.
    LineSuffix(Doc<'a>),
    /// Forces any containing [`Group`](DocKind::Group) to render in [`Mode::Break`].
    ///
    /// Unlike [`Doc::hardline`], this does not itself insert a line break. It is useful for cases
    /// where a descendant requires a parent to break (e.g. certain comment layouts) without
    /// unconditionally emitting a hard newline.
    BreakParent,
    /// A "fill" document packs parts onto the current line until they no longer fit, then
    /// continues on the next line.
    ///
    /// It is modeled after Prettier's `fill` primitive and expects `parts` to alternate between
    /// "content" docs and "separator" docs, e.g.:
    ///
    /// ```text
    /// [item, line, item, line, item]
    /// ```
    Fill(Vec<Doc<'a>>),
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

    /// Returns `true` if this doc is [`Doc::nil()`].
    #[inline]
    pub fn is_nil(&self) -> bool {
        matches!(self.kind(), DocKind::Nil)
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

    /// A line break that is always rendered as `config.newline`.
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

    /// Attach `doc` as a line suffix to be rendered at the end of the current line.
    ///
    /// This is the primary building block for correct trailing line comment behavior.
    pub fn line_suffix(doc: Doc<'a>) -> Self {
        Self::new(DocKind::LineSuffix(doc))
    }

    /// Force the parent group to break.
    pub fn break_parent() -> Self {
        Self::new(DocKind::BreakParent)
    }

    /// Greedily pack `parts` onto a line, breaking as needed.
    ///
    /// See [`DocKind::Fill`] for expected structure.
    pub fn fill<I>(parts: I) -> Self
    where
        I: IntoIterator<Item = Doc<'a>>,
    {
        let mut items = Vec::new();
        for part in parts {
            if matches!(part.kind(), DocKind::Nil) {
                continue;
            }
            items.push(part);
        }

        match items.len() {
            0 => Self::nil(),
            1 => items.pop().unwrap(),
            _ => Self::new(DocKind::Fill(items)),
        }
    }

    /// Join `docs` with `separator` between each element.
    pub fn join<I>(separator: Doc<'a>, docs: I) -> Self
    where
        I: IntoIterator<Item = Doc<'a>>,
    {
        let mut parts = Vec::new();
        for doc in docs.into_iter() {
            if matches!(doc.kind(), DocKind::Nil) {
                continue;
            }
            if !parts.is_empty() {
                parts.push(separator.clone());
            }
            parts.push(doc);
        }
        Self::concat(parts)
    }
}

#[derive(Clone, Debug)]
enum Command<'a> {
    Doc {
        indent: usize,
        mode: Mode,
        doc: Doc<'a>,
    },
    Fill {
        indent: usize,
        mode: Mode,
        doc: Doc<'a>,
        index: usize,
    },
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

    let mut stack = vec![Command::Doc {
        indent: 0,
        mode: Mode::Break,
        doc,
    }];

    let mut line_suffixes: Vec<Command<'a>> = Vec::new();

    while !stack.is_empty() || !line_suffixes.is_empty() {
        if stack.is_empty() {
            flush_line_suffixes(&mut stack, &mut line_suffixes);
            continue;
        }

        match stack.pop().expect("stack is not empty") {
            Command::Doc { indent, mode, doc } => match doc.kind() {
                DocKind::Nil => {}
                DocKind::Text(text) => {
                    out.push_str(text);
                    pos = pos.saturating_add(text_width(text));
                }
                DocKind::Concat(parts) => {
                    for part in parts.iter().rev() {
                        stack.push(Command::Doc {
                            indent,
                            mode,
                            doc: part.clone(),
                        });
                    }
                }
                DocKind::Group(inner) => match mode {
                    Mode::Flat => stack.push(Command::Doc {
                        indent,
                        mode: Mode::Flat,
                        doc: inner.clone(),
                    }),
                    Mode::Break => {
                        let remaining_width = config.max_width as isize - pos as isize;
                        let lookahead = vec![Command::Doc {
                            indent,
                            mode: Mode::Flat,
                            doc: inner.clone(),
                        }];

                        let next_mode =
                            if fits(remaining_width, &stack, &line_suffixes, &lookahead, config) {
                                Mode::Flat
                            } else {
                                Mode::Break
                            };
                        stack.push(Command::Doc {
                            indent,
                            mode: next_mode,
                            doc: inner.clone(),
                        });
                    }
                },
                DocKind::Nest(spaces, inner) => stack.push(Command::Doc {
                    indent: indent.saturating_add(*spaces),
                    mode,
                    doc: inner.clone(),
                }),
                DocKind::Indent(inner) => stack.push(Command::Doc {
                    indent: indent.saturating_add(config.indent_width),
                    mode,
                    doc: inner.clone(),
                }),
                DocKind::Line(kind) => {
                    let will_break = match mode {
                        Mode::Break => true,
                        Mode::Flat => matches!(kind, LineKind::Hard),
                    };

                    if will_break && !line_suffixes.is_empty() {
                        stack.push(Command::Doc {
                            indent,
                            mode,
                            doc: doc.clone(),
                        });
                        flush_line_suffixes(&mut stack, &mut line_suffixes);
                        continue;
                    }

                    match mode {
                        Mode::Flat => match kind {
                            LineKind::Line => {
                                out.push(' ');
                                pos = pos.saturating_add(1);
                            }
                            LineKind::Soft => {}
                            LineKind::Hard => {
                                trim_trailing_whitespace(&mut out);
                                out.push_str(config.newline);
                                push_spaces(&mut out, indent);
                                pos = indent;
                            }
                        },
                        Mode::Break => {
                            trim_trailing_whitespace(&mut out);
                            out.push_str(config.newline);
                            push_spaces(&mut out, indent);
                            pos = indent;
                        }
                    }
                }
                DocKind::LineSuffix(inner) => {
                    line_suffixes.push(Command::Doc {
                        indent,
                        mode,
                        doc: inner.clone(),
                    });
                }
                DocKind::BreakParent => {}
                DocKind::Fill(_) => stack.push(Command::Fill {
                    indent,
                    mode,
                    doc: doc.clone(),
                    index: 0,
                }),
                DocKind::IfBreak {
                    break_doc,
                    flat_doc,
                } => {
                    let chosen = if mode == Mode::Break {
                        break_doc.clone()
                    } else {
                        flat_doc.clone()
                    };
                    stack.push(Command::Doc {
                        indent,
                        mode,
                        doc: chosen,
                    });
                }
            },
            Command::Fill {
                indent,
                mode,
                doc,
                index,
            } => {
                let DocKind::Fill(parts) = doc.kind() else {
                    unreachable!("Fill command must reference DocKind::Fill")
                };

                if index >= parts.len() {
                    continue;
                }

                match mode {
                    Mode::Flat => {
                        for part in parts[index..].iter().rev() {
                            stack.push(Command::Doc {
                                indent,
                                mode: Mode::Flat,
                                doc: part.clone(),
                            });
                        }
                    }
                    Mode::Break => {
                        if index % 2 == 0 {
                            // Content part.
                            stack.push(Command::Fill {
                                indent,
                                mode,
                                doc: doc.clone(),
                                index: index + 1,
                            });
                            stack.push(Command::Doc {
                                indent,
                                mode,
                                doc: parts[index].clone(),
                            });
                            continue;
                        }

                        // Separator part.
                        let sep = parts[index].clone();
                        if index + 1 >= parts.len() {
                            stack.push(Command::Doc {
                                indent,
                                mode,
                                doc: sep,
                            });
                            continue;
                        }

                        let next = parts[index + 1].clone();
                        let remaining_width = config.max_width as isize - pos as isize;
                        let sep_mode = if fits_flat(remaining_width, &[sep.clone(), next.clone()]) {
                            Mode::Flat
                        } else {
                            Mode::Break
                        };

                        stack.push(Command::Fill {
                            indent,
                            mode,
                            doc: doc.clone(),
                            index: index + 2,
                        });
                        stack.push(Command::Doc {
                            indent,
                            mode,
                            doc: next,
                        });
                        stack.push(Command::Doc {
                            indent,
                            mode: sep_mode,
                            doc: sep,
                        });
                    }
                }
            }
        }
    }

    out
}

fn push_spaces(out: &mut String, count: usize) {
    out.extend(std::iter::repeat_n(' ', count));
}

fn trim_trailing_whitespace(out: &mut String) {
    while matches!(out.as_bytes().last(), Some(b' ' | b'\t')) {
        out.pop();
    }
}

fn text_width(text: &str) -> usize {
    if text.is_ascii() {
        text.len()
    } else {
        text.chars().count()
    }
}

fn flush_line_suffixes<'a>(stack: &mut Vec<Command<'a>>, line_suffixes: &mut Vec<Command<'a>>) {
    // `line_suffixes` is stored in insertion order. We push them onto the stack in reverse so they
    // are popped/printed in the original order.
    for cmd in line_suffixes.drain(..).rev() {
        stack.push(cmd);
    }
}

fn fits_flat<'a>(mut remaining_width: isize, docs: &[Doc<'a>]) -> bool {
    if remaining_width < 0 {
        return false;
    }

    // This is an intentionally small cap: `fits_flat` is used as an inner primitive for `fill`
    // decisions. If we hit the cap, treat as "doesn't fit" to stay deterministic and avoid
    // quadratic blowups.
    const MAX_STEPS: usize = 4_096;
    let mut steps = 0usize;

    let mut stack: Vec<Doc<'a>> = docs.iter().cloned().rev().collect();

    while remaining_width >= 0 {
        if steps >= MAX_STEPS {
            return false;
        }
        steps += 1;

        let Some(doc) = stack.pop() else {
            return true;
        };

        match doc.kind() {
            DocKind::Nil => {}
            DocKind::Text(text) => remaining_width -= text_width(text) as isize,
            DocKind::Concat(parts) => {
                for part in parts.iter().rev() {
                    stack.push(part.clone());
                }
            }
            DocKind::Group(inner) => stack.push(inner.clone()),
            DocKind::Nest(_, inner) => stack.push(inner.clone()),
            DocKind::Indent(inner) => stack.push(inner.clone()),
            DocKind::Line(kind) => match kind {
                LineKind::Line => remaining_width -= 1,
                LineKind::Soft => {}
                LineKind::Hard => return false,
            },
            // Line suffix docs are ignored for fitting. This matches Prettier's behavior where
            // long trailing comments should not force the surrounding code to wrap.
            DocKind::LineSuffix(_inner) => {}
            DocKind::BreakParent => return false,
            DocKind::Fill(parts) => {
                for part in parts.iter().rev() {
                    stack.push(part.clone());
                }
            }
            DocKind::IfBreak { flat_doc, .. } => stack.push(flat_doc.clone()),
        }
    }

    false
}

fn fits<'a>(
    mut remaining_width: isize,
    base_stack: &[Command<'a>],
    _initial_line_suffixes: &[Command<'a>],
    lookahead: &[Command<'a>],
    config: PrintConfig,
) -> bool {
    if remaining_width < 0 {
        return false;
    }

    // Cap the amount of work `fits` can do to avoid pathological O(n^2) behavior with deeply
    // nested groups. If we hit the cap, prefer breaking to keep output deterministic.
    const MAX_STEPS: usize = 32_768;
    let mut steps = 0usize;

    let mut idx = base_stack.len();
    let mut stack: Vec<Command<'a>> = lookahead.to_vec();
    // Like Prettier, `lineSuffix` should not affect line-fitting decisions. In particular,
    // trailing `//` comments may be arbitrarily long and must not force surrounding groups to
    // choose a broken layout.
    let mut line_suffixes: Vec<Command<'a>> = Vec::new();

    while remaining_width >= 0 {
        if steps >= MAX_STEPS {
            return false;
        }
        steps += 1;

        let cmd = if let Some(cmd) = stack.pop() {
            cmd
        } else if idx > 0 {
            idx -= 1;
            base_stack[idx].clone()
        } else if !line_suffixes.is_empty() {
            flush_line_suffixes(&mut stack, &mut line_suffixes);
            continue;
        } else {
            return true;
        };

        match cmd {
            Command::Doc { indent, mode, doc } => match doc.kind() {
                DocKind::Nil => {}
                DocKind::Text(text) => remaining_width -= text_width(text) as isize,
                DocKind::Concat(parts) => {
                    for part in parts.iter().rev() {
                        stack.push(Command::Doc {
                            indent,
                            mode,
                            doc: part.clone(),
                        });
                    }
                }
                DocKind::Group(inner) => stack.push(Command::Doc {
                    indent,
                    mode,
                    doc: inner.clone(),
                }),
                DocKind::Nest(spaces, inner) => stack.push(Command::Doc {
                    indent: indent.saturating_add(*spaces),
                    mode,
                    doc: inner.clone(),
                }),
                DocKind::Indent(inner) => stack.push(Command::Doc {
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
                    Mode::Break => {
                        if !line_suffixes.is_empty() {
                            stack.push(Command::Doc {
                                indent,
                                mode,
                                doc: doc.clone(),
                            });
                            flush_line_suffixes(&mut stack, &mut line_suffixes);
                            continue;
                        }
                        return true;
                    }
                },
                DocKind::LineSuffix(_inner) => {
                    // Line suffixes do not participate in fitting calculations.
                }
                DocKind::BreakParent => {
                    if mode == Mode::Flat {
                        return false;
                    }
                }
                DocKind::Fill(_) => stack.push(Command::Fill {
                    indent,
                    mode,
                    doc: doc.clone(),
                    index: 0,
                }),
                DocKind::IfBreak {
                    break_doc,
                    flat_doc,
                } => {
                    let chosen = if mode == Mode::Break {
                        break_doc.clone()
                    } else {
                        flat_doc.clone()
                    };
                    stack.push(Command::Doc {
                        indent,
                        mode,
                        doc: chosen,
                    });
                }
            },
            Command::Fill {
                indent,
                mode,
                doc,
                index,
            } => {
                let DocKind::Fill(parts) = doc.kind() else {
                    unreachable!("Fill command must reference DocKind::Fill")
                };

                if index >= parts.len() {
                    continue;
                }

                match mode {
                    Mode::Flat => {
                        for part in parts[index..].iter().rev() {
                            stack.push(Command::Doc {
                                indent,
                                mode: Mode::Flat,
                                doc: part.clone(),
                            });
                        }
                    }
                    Mode::Break => {
                        if index % 2 == 0 {
                            stack.push(Command::Fill {
                                indent,
                                mode,
                                doc: doc.clone(),
                                index: index + 1,
                            });
                            stack.push(Command::Doc {
                                indent,
                                mode,
                                doc: parts[index].clone(),
                            });
                            continue;
                        }

                        let sep = parts[index].clone();
                        if index + 1 >= parts.len() {
                            stack.push(Command::Doc {
                                indent,
                                mode,
                                doc: sep,
                            });
                            continue;
                        }

                        let next = parts[index + 1].clone();
                        let sep_mode = if fits_flat(remaining_width, &[sep.clone(), next.clone()]) {
                            Mode::Flat
                        } else {
                            Mode::Break
                        };

                        stack.push(Command::Fill {
                            indent,
                            mode,
                            doc: doc.clone(),
                            index: index + 2,
                        });
                        stack.push(Command::Doc {
                            indent,
                            mode,
                            doc: next,
                        });
                        stack.push(Command::Doc {
                            indent,
                            mode: sep_mode,
                            doc: sep,
                        });
                    }
                }
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
            newline: "\n",
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

    #[test]
    fn respects_configured_newline() {
        let doc = Doc::concat([Doc::text("a"), Doc::hardline(), Doc::text("b")]);
        let cfg = PrintConfig {
            max_width: 100,
            indent_width: 4,
            newline: "\r\n",
        };
        assert_eq!(print(doc, cfg), "a\r\nb");
    }

    #[test]
    fn break_parent_forces_group_to_break() {
        let doc = Doc::concat([
            Doc::text("a"),
            Doc::break_parent(),
            Doc::line(),
            Doc::text("b"),
        ])
        .group();

        // `break_parent` prevents a group from rendering flat even when it would otherwise fit.
        assert_eq!(print(doc, cfg(100)), "a\nb");
    }

    #[test]
    fn fill_packs_until_it_does_not_fit() {
        let doc = Doc::fill([
            Doc::text("a"),
            Doc::line(),
            Doc::text("b"),
            Doc::line(),
            Doc::text("c"),
        ]);

        assert_eq!(print(doc.clone(), cfg(100)), "a b c");
        assert_eq!(print(doc, cfg(3)), "a b\nc");
    }

    #[test]
    fn line_suffix_flushes_before_newline() {
        let args = Doc::concat([Doc::text("arg1,"), Doc::line(), Doc::text("arg2")]);
        let call = Doc::concat([
            Doc::text("call("),
            Doc::concat([Doc::softline(), args]).indent(),
            Doc::softline(),
            Doc::text(")"),
        ])
        .group();

        let doc = Doc::concat([
            call,
            Doc::line_suffix(Doc::text(" // trailing")),
            Doc::hardline(),
            Doc::text("next"),
        ]);

        assert_eq!(
            print(doc.clone(), cfg(100)),
            "call(arg1, arg2) // trailing\nnext"
        );
        assert_eq!(
            print(doc, cfg(10)),
            "call(\n    arg1,\n    arg2\n) // trailing\nnext"
        );
    }
}
