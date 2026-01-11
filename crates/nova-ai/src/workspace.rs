use crate::patch::{Patch, TextEdit, UnifiedDiffHunk, UnifiedDiffLine, UnifiedDiffPatch};
use nova_core::{LineIndex, Position as CorePosition, TextRange, TextSize};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct VirtualWorkspace {
    files: BTreeMap<String, String>,
}

impl VirtualWorkspace {
    pub fn new(files: impl IntoIterator<Item = (String, String)>) -> Self {
        Self {
            files: files.into_iter().collect(),
        }
    }

    pub fn get(&self, path: &str) -> Option<&str> {
        self.files.get(path).map(String::as_str)
    }

    pub fn insert(&mut self, path: impl Into<String>, contents: impl Into<String>) {
        self.files.insert(path.into(), contents.into());
    }

    pub fn files(&self) -> impl Iterator<Item = (&String, &String)> {
        self.files.iter()
    }

    pub fn apply_patch(&self, patch: &Patch) -> Result<AppliedPatch, PatchApplyError> {
        match patch {
            Patch::Edits(edits) => self.apply_text_edits(edits),
            Patch::UnifiedDiff(diff) => self.apply_unified_diff(diff),
        }
    }

    fn apply_text_edits(&self, edits: &[TextEdit]) -> Result<AppliedPatch, PatchApplyError> {
        let mut files: BTreeMap<String, Vec<&TextEdit>> = BTreeMap::new();
        for edit in edits {
            files.entry(edit.file.clone()).or_default().push(edit);
        }

        let mut out = self.clone();
        let mut touched_ranges: BTreeMap<String, Vec<TextRange>> = BTreeMap::new();

        for (file, file_edits) in files {
            let original = out.files.get(&file).map(String::as_str).unwrap_or("");
            let (new_text, ranges) = apply_edits_to_text(original, file_edits)?;
            out.files.insert(file.clone(), new_text);
            touched_ranges.insert(file, ranges);
        }

        Ok(AppliedPatch {
            workspace: out,
            touched_ranges,
        })
    }

    fn apply_unified_diff(&self, diff: &UnifiedDiffPatch) -> Result<AppliedPatch, PatchApplyError> {
        let mut out = self.clone();
        let mut touched_ranges = BTreeMap::new();

        for file in &diff.files {
            if file.new_path == "/dev/null" {
                return Err(PatchApplyError::InvalidUnifiedDiff(
                    "file deletions are not supported".into(),
                ));
            }
            if file.old_path != "/dev/null" && file.old_path != file.new_path {
                return Err(PatchApplyError::InvalidUnifiedDiff(
                    "file renames are not supported".into(),
                ));
            }

            let original = if file.old_path == "/dev/null" {
                ""
            } else {
                out.files
                    .get(&file.new_path)
                    .map(String::as_str)
                    .ok_or_else(|| PatchApplyError::MissingFile {
                        file: file.new_path.clone(),
                    })?
            };

            let new_text = apply_unified_diff_hunks(original, &file.hunks)?;
            out.files.insert(file.new_path.clone(), new_text);
            let len = out
                .files
                .get(&file.new_path)
                .map(|text| text.len())
                .unwrap_or(0);
            touched_ranges.insert(
                file.new_path.clone(),
                vec![TextRange::new(
                    TextSize::from(0),
                    TextSize::from(len as u32),
                )],
            );
        }

        Ok(AppliedPatch {
            workspace: out,
            touched_ranges,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedPatch {
    pub workspace: VirtualWorkspace,
    pub touched_ranges: BTreeMap<String, Vec<TextRange>>,
}

#[derive(Debug, Error)]
pub enum PatchApplyError {
    #[error("invalid text edit range for file '{file}'")]
    InvalidRange { file: String },
    #[error("overlapping edits detected for file '{file}'")]
    OverlappingEdits { file: String },
    #[error("file '{file}' was not present in the workspace")]
    MissingFile { file: String },
    #[error("invalid unified diff: {0}")]
    InvalidUnifiedDiff(String),
    #[error("unified diff did not apply cleanly: {0}")]
    UnifiedDiffApplyFailed(String),
}

struct OffsetEdit<'a> {
    file: &'a str,
    start: usize,
    end: usize,
    insert: &'a str,
}

fn apply_edits_to_text(
    original: &str,
    edits: Vec<&TextEdit>,
) -> Result<(String, Vec<TextRange>), PatchApplyError> {
    let mut offset_edits = Vec::with_capacity(edits.len());
    let index = LineIndex::new(original);
    for edit in &edits {
        let start_pos = CorePosition::new(edit.range.start.line, edit.range.start.character);
        let end_pos = CorePosition::new(edit.range.end.line, edit.range.end.character);

        let start = index
            .offset_of_position(original, start_pos)
            .map(text_size_to_usize)
            .ok_or_else(|| PatchApplyError::InvalidRange {
                file: edit.file.clone(),
            })?;
        let end = index
            .offset_of_position(original, end_pos)
            .map(text_size_to_usize)
            .ok_or_else(|| PatchApplyError::InvalidRange {
                file: edit.file.clone(),
            })?;
        if start > end {
            return Err(PatchApplyError::InvalidRange {
                file: edit.file.clone(),
            });
        }

        offset_edits.push(OffsetEdit {
            file: &edit.file,
            start,
            end,
            insert: &edit.text,
        });
    }

    offset_edits.sort_by_key(|edit| edit.start);

    let mut last_end = 0usize;
    for edit in &offset_edits {
        if edit.start < last_end {
            return Err(PatchApplyError::OverlappingEdits {
                file: edit.file.to_string(),
            });
        }
        last_end = edit.end;
    }

    let mut out = String::with_capacity(
        original.len().saturating_add(
            offset_edits
                .iter()
                .map(|edit| edit.insert.len())
                .sum::<usize>(),
        ),
    );

    let mut cursor = 0usize;
    let mut inserted_spans: Vec<(usize, usize)> = Vec::with_capacity(offset_edits.len());
    for edit in &offset_edits {
        out.push_str(&original[cursor..edit.start]);
        let start_offset = out.len();
        out.push_str(edit.insert);
        let end_offset = out.len();
        inserted_spans.push((start_offset, end_offset));
        cursor = edit.end;
    }
    out.push_str(&original[cursor..]);

    let touched_ranges = inserted_spans
        .into_iter()
        .map(|(start, end)| TextRange::new(text_size_from_usize(start), text_size_from_usize(end)))
        .collect();

    Ok((out, touched_ranges))
}

fn text_size_to_usize(size: TextSize) -> usize {
    u32::from(size) as usize
}

fn text_size_from_usize(offset: usize) -> TextSize {
    TextSize::from(offset as u32)
}
fn apply_unified_diff_hunks(
    original: &str,
    hunks: &[UnifiedDiffHunk],
) -> Result<String, PatchApplyError> {
    let original_lines: Vec<&str> = original.lines().collect();

    let mut output_lines: Vec<String> = Vec::new();
    let mut cursor = 1usize; // diff is 1-based

    for hunk in hunks {
        let old_start = if hunk.old_start == 0 {
            1
        } else {
            hunk.old_start
        };

        if old_start < cursor {
            return Err(PatchApplyError::UnifiedDiffApplyFailed(
                "overlapping hunks".into(),
            ));
        }

        let expected_new_prefix = hunk.new_start.saturating_sub(1);
        let prefix_end = old_start.saturating_sub(1);
        for idx in cursor..=prefix_end {
            if let Some(line) = original_lines.get(idx.saturating_sub(1)) {
                output_lines.push((*line).to_string());
            } else {
                return Err(PatchApplyError::UnifiedDiffApplyFailed(
                    "hunk start beyond end of file".into(),
                ));
            }
        }

        if output_lines.len() != expected_new_prefix {
            return Err(PatchApplyError::UnifiedDiffApplyFailed(
                "new hunk start does not match output position".into(),
            ));
        }

        let mut old_index = old_start;
        let mut old_consumed = 0usize;
        let mut new_consumed = 0usize;

        for line in &hunk.lines {
            match line {
                UnifiedDiffLine::Context(text) => {
                    let original_line = original_lines
                        .get(old_index.saturating_sub(1))
                        .ok_or_else(|| {
                            PatchApplyError::UnifiedDiffApplyFailed(
                                "context beyond end of file".into(),
                            )
                        })?;
                    if original_line != text {
                        return Err(PatchApplyError::UnifiedDiffApplyFailed(format!(
                            "context mismatch at line {old_index}"
                        )));
                    }
                    output_lines.push(text.clone());
                    old_index += 1;
                    old_consumed += 1;
                    new_consumed += 1;
                }
                UnifiedDiffLine::Remove(text) => {
                    let original_line = original_lines
                        .get(old_index.saturating_sub(1))
                        .ok_or_else(|| {
                            PatchApplyError::UnifiedDiffApplyFailed(
                                "remove beyond end of file".into(),
                            )
                        })?;
                    if original_line != text {
                        return Err(PatchApplyError::UnifiedDiffApplyFailed(format!(
                            "remove mismatch at line {old_index}"
                        )));
                    }
                    old_index += 1;
                    old_consumed += 1;
                }
                UnifiedDiffLine::Add(text) => {
                    output_lines.push(text.clone());
                    new_consumed += 1;
                }
            }
        }

        if old_consumed != hunk.old_len || new_consumed != hunk.new_len {
            return Err(PatchApplyError::UnifiedDiffApplyFailed(
                "hunk length does not match header".into(),
            ));
        }

        cursor = old_index;
    }

    for idx in cursor..=original_lines.len() {
        if let Some(line) = original_lines.get(idx.saturating_sub(1)) {
            output_lines.push((*line).to_string());
        }
    }

    Ok(output_lines.join("\n"))
}

pub fn affected_files(patch: &Patch) -> BTreeSet<String> {
    match patch {
        Patch::Edits(edits) => edits.iter().map(|edit| edit.file.clone()).collect(),
        Patch::UnifiedDiff(diff) => diff
            .files
            .iter()
            .map(|file| file.new_path.clone())
            .collect(),
    }
}
