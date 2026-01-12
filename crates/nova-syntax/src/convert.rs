use crate::{TextEdit, TextRange};

/// Errors that can occur when converting `nova_core` text edit primitives into
/// the `nova_syntax` incremental parsing equivalents.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TextEditConvertError {
    #[error("text range start {start} is not representable as u32")]
    StartNotRepresentable { start: u64 },
    #[error("text range end {end} is not representable as u32")]
    EndNotRepresentable { end: u64 },
    #[error("invalid text range ordering: start {start} > end {end}")]
    InvalidOrdering { start: u32, end: u32 },
}

fn try_range_from_u64(start: u64, end: u64) -> Result<TextRange, TextEditConvertError> {
    let start_u32: u32 = start
        .try_into()
        .map_err(|_| TextEditConvertError::StartNotRepresentable { start })?;
    let end_u32: u32 = end
        .try_into()
        .map_err(|_| TextEditConvertError::EndNotRepresentable { end })?;
    if start_u32 > end_u32 {
        return Err(TextEditConvertError::InvalidOrdering {
            start: start_u32,
            end: end_u32,
        });
    }
    Ok(TextRange {
        start: start_u32,
        end: end_u32,
    })
}

impl TryFrom<nova_core::TextRange> for TextRange {
    type Error = TextEditConvertError;

    fn try_from(value: nova_core::TextRange) -> Result<Self, Self::Error> {
        // `nova_core::TextRange` uses `text_size` offsets. We explicitly roundtrip through a
        // larger integer type so the conversion remains obviously-checked even if core text
        // offsets grow beyond `u32` in the future.
        let start = u64::from(u32::from(value.start()));
        let end = u64::from(u32::from(value.end()));
        try_range_from_u64(start, end)
    }
}

impl TryFrom<&nova_core::TextRange> for TextRange {
    type Error = TextEditConvertError;

    fn try_from(value: &nova_core::TextRange) -> Result<Self, Self::Error> {
        (*value).try_into()
    }
}

impl TryFrom<nova_core::TextEdit> for TextEdit {
    type Error = TextEditConvertError;

    fn try_from(value: nova_core::TextEdit) -> Result<Self, Self::Error> {
        Ok(TextEdit {
            range: value.range.try_into()?,
            replacement: value.replacement,
        })
    }
}

impl TryFrom<&nova_core::TextEdit> for TextEdit {
    type Error = TextEditConvertError;

    fn try_from(value: &nova_core::TextEdit) -> Result<Self, Self::Error> {
        Ok(TextEdit {
            range: value.range.try_into()?,
            replacement: value.replacement.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nova_core::{TextEdit as CoreTextEdit, TextRange as CoreTextRange, TextSize};

    #[test]
    fn converts_core_text_edits_to_syntax_text_edits() {
        let edits = vec![
            // Replace "cd" -> "XX"
            CoreTextEdit::new(
                CoreTextRange::new(TextSize::from(2), TextSize::from(4)),
                "XX",
            ),
            // Insert "!" at start
            CoreTextEdit::insert(TextSize::from(0), "!"),
            // Delete "f"
            CoreTextEdit::new(CoreTextRange::new(TextSize::from(5), TextSize::from(6)), ""),
        ];

        let converted: Vec<TextEdit> = edits
            .into_iter()
            .map(|edit| TextEdit::try_from(edit).unwrap())
            .collect();

        assert_eq!(
            converted,
            vec![
                TextEdit::new(TextRange { start: 2, end: 4 }, "XX"),
                TextEdit::insert(0, "!"),
                TextEdit::new(TextRange { start: 5, end: 6 }, ""),
            ]
        );
    }

    #[test]
    fn converts_borrowed_core_text_edits_to_syntax_text_edits() {
        let edit = CoreTextEdit::new(
            CoreTextRange::new(TextSize::from(2), TextSize::from(4)),
            "XX",
        );
        let converted = TextEdit::try_from(&edit).unwrap();
        assert_eq!(converted.replacement, "XX");
        assert_eq!(converted.range, TextRange { start: 2, end: 4 });
    }

    #[test]
    fn rejects_offsets_that_do_not_fit_in_u32() {
        let too_large = u64::from(u32::MAX) + 1;
        assert!(matches!(
            try_range_from_u64(too_large, too_large),
            Err(TextEditConvertError::StartNotRepresentable { start }) if start == too_large
        ));

        assert!(matches!(
            try_range_from_u64(0, too_large),
            Err(TextEditConvertError::EndNotRepresentable { end }) if end == too_large
        ));
    }

    #[test]
    fn rejects_invalid_range_ordering() {
        assert!(matches!(
            try_range_from_u64(10, 9),
            Err(TextEditConvertError::InvalidOrdering { start: 10, end: 9 })
        ));
    }
}
