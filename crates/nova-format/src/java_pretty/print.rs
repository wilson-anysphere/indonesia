use crate::doc::Doc;

pub(crate) fn space<'a>() -> Doc<'a> {
    Doc::text(" ")
}

#[allow(dead_code)]
pub(crate) fn hardline<'a>() -> Doc<'a> {
    Doc::hardline()
}

#[allow(dead_code)]
pub(crate) fn join_with_hardline<'a>(docs: impl IntoIterator<Item = Doc<'a>>) -> Doc<'a> {
    let mut parts = Vec::new();
    for (idx, doc) in docs.into_iter().enumerate() {
        if idx > 0 {
            parts.push(Doc::hardline());
        }
        parts.push(doc);
    }
    Doc::concat(parts)
}
