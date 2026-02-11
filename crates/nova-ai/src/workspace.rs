use crate::patch::{
    JsonPatch, JsonPatchOp, Patch, TextEdit, UnifiedDiffHunk, UnifiedDiffLine, UnifiedDiffPatch,
};
use nova_core::{LineIndex, Position as CorePosition, TextRange, TextSize};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PatchApplyConfig {
    pub allow_new_files: bool,
}

impl Default for PatchApplyConfig {
    fn default() -> Self {
        Self {
            allow_new_files: false,
        }
    }
}

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
        self.apply_patch_with_config(patch, &PatchApplyConfig::default())
    }

    pub fn apply_patch_with_config(
        &self,
        patch: &Patch,
        config: &PatchApplyConfig,
    ) -> Result<AppliedPatch, PatchApplyError> {
        match patch {
            Patch::Json(patch) => self.apply_json_patch(patch, config),
            Patch::UnifiedDiff(diff) => self.apply_unified_diff(diff, config),
        }
    }

    fn apply_json_patch(
        &self,
        patch: &JsonPatch,
        config: &PatchApplyConfig,
    ) -> Result<AppliedPatch, PatchApplyError> {
        let mut out = self.clone();
        let mut touched_ranges: BTreeMap<String, Vec<TextRange>> = BTreeMap::new();
        let mut created_files = BTreeSet::new();
        let mut deleted_files = BTreeSet::new();
        let mut renamed_files: BTreeMap<String, String> = BTreeMap::new();

        for op in &patch.ops {
            match op {
                JsonPatchOp::Create { file, text } => {
                    if !config.allow_new_files {
                        return Err(PatchApplyError::NewFileNotAllowed { file: file.clone() });
                    }
                    if out.files.contains_key(file) {
                        return Err(PatchApplyError::FileAlreadyExists { file: file.clone() });
                    }
                    out.files.insert(file.clone(), text.clone());
                    created_files.insert(file.clone());
                    if !text.is_empty() {
                        touched_ranges.insert(
                            file.clone(),
                            vec![TextRange::new(
                                TextSize::from(0),
                                TextSize::from(text.len() as u32),
                            )],
                        );
                    }
                }
                JsonPatchOp::Delete { file } => {
                    if out.files.remove(file).is_none() {
                        return Err(PatchApplyError::MissingFile { file: file.clone() });
                    }
                    created_files.remove(file);
                    touched_ranges.remove(file);
                    deleted_files.insert(file.clone());
                }
                JsonPatchOp::Rename { from, to } => {
                    if out.files.contains_key(to) {
                        return Err(PatchApplyError::FileAlreadyExists { file: to.clone() });
                    }
                    let contents = out
                        .files
                        .remove(from)
                        .ok_or_else(|| PatchApplyError::MissingFile { file: from.clone() })?;
                    out.files.insert(to.clone(), contents);
                    renamed_files.insert(to.clone(), from.clone());
                    if let Some(ranges) = touched_ranges.remove(from) {
                        touched_ranges.insert(to.clone(), ranges);
                    }
                    if created_files.remove(from) {
                        created_files.insert(to.clone());
                    }
                }
            }
        }

        if !patch.edits.is_empty() {
            let edit_ranges = apply_text_edits_into(
                &mut out,
                &patch.edits,
                config,
                &mut created_files,
                &mut deleted_files,
            )?;
            for (file, ranges) in edit_ranges {
                touched_ranges.entry(file).or_default().extend(ranges);
            }
        }

        Ok(AppliedPatch {
            workspace: out,
            touched_ranges,
            created_files,
            deleted_files,
            renamed_files,
        })
    }

    fn apply_unified_diff(
        &self,
        diff: &UnifiedDiffPatch,
        config: &PatchApplyConfig,
    ) -> Result<AppliedPatch, PatchApplyError> {
        let mut out = self.clone();
        let mut touched_ranges: BTreeMap<String, Vec<TextRange>> = BTreeMap::new();
        let mut created_files = BTreeSet::new();
        let mut deleted_files = BTreeSet::new();
        let mut renamed_files: BTreeMap<String, String> = BTreeMap::new();

        for file in &diff.files {
            let (op, source_path, target_path) = classify_unified_diff_file(file)?;
            match op {
                UnifiedDiffFileOp::Create => {
                    if !config.allow_new_files {
                        return Err(PatchApplyError::NewFileNotAllowed { file: target_path });
                    }
                    if out.files.contains_key(&target_path) {
                        return Err(PatchApplyError::FileAlreadyExists { file: target_path });
                    }
                    let style = infer_text_style_for_new_file(&out, &target_path);
                    let (new_text, applied) =
                        apply_unified_diff_hunks("", &file.hunks, style, &target_path)?;
                    out.files.insert(target_path.clone(), new_text.clone());
                    created_files.insert(target_path.clone());
                    let ranges = approximate_hunk_ranges(&new_text, &applied);
                    if !ranges.is_empty() {
                        touched_ranges
                            .entry(target_path.clone())
                            .or_default()
                            .extend(ranges);
                    }
                }
                UnifiedDiffFileOp::Delete => {
                    let original =
                        out.files
                            .get(&source_path)
                            .map(String::as_str)
                            .ok_or_else(|| PatchApplyError::MissingFile {
                                file: source_path.clone(),
                            })?;
                    let style = TextStyle::from_original(original);
                    let (_new_text, _applied) =
                        apply_unified_diff_hunks(original, &file.hunks, style, &source_path)?;
                    out.files.remove(&source_path);
                    deleted_files.insert(source_path);
                }
                UnifiedDiffFileOp::Rename => {
                    if out.files.contains_key(&target_path) {
                        return Err(PatchApplyError::FileAlreadyExists { file: target_path });
                    }
                    let original =
                        out.files
                            .get(&source_path)
                            .map(String::as_str)
                            .ok_or_else(|| PatchApplyError::MissingFile {
                                file: source_path.clone(),
                            })?;
                    let style = TextStyle::from_original(original);
                    let (new_text, applied) =
                        apply_unified_diff_hunks(original, &file.hunks, style, &source_path)?;
                    out.files.remove(&source_path);
                    out.files.insert(target_path.clone(), new_text.clone());
                    renamed_files.insert(target_path.clone(), source_path);
                    let ranges = approximate_hunk_ranges(&new_text, &applied);
                    if !ranges.is_empty() {
                        touched_ranges
                            .entry(target_path.clone())
                            .or_default()
                            .extend(ranges);
                    }
                }
                UnifiedDiffFileOp::Modify => {
                    let original =
                        out.files
                            .get(&target_path)
                            .map(String::as_str)
                            .ok_or_else(|| PatchApplyError::MissingFile {
                                file: target_path.clone(),
                            })?;
                    let style = TextStyle::from_original(original);
                    let (new_text, applied) =
                        apply_unified_diff_hunks(original, &file.hunks, style, &target_path)?;
                    out.files.insert(target_path.clone(), new_text.clone());
                    let ranges = approximate_hunk_ranges(&new_text, &applied);
                    if !ranges.is_empty() {
                        touched_ranges
                            .entry(target_path.clone())
                            .or_default()
                            .extend(ranges);
                    }
                }
            }
        }

        Ok(AppliedPatch {
            workspace: out,
            touched_ranges,
            created_files,
            deleted_files,
            renamed_files,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedPatch {
    pub workspace: VirtualWorkspace,
    pub touched_ranges: BTreeMap<String, Vec<TextRange>>,
    pub created_files: BTreeSet<String>,
    pub deleted_files: BTreeSet<String>,
    /// Mapping from new path -> old path for renames.
    pub renamed_files: BTreeMap<String, String>,
}

#[derive(Debug, Error)]
pub enum PatchApplyError {
    #[error("invalid text edit range for file '{file}'")]
    InvalidRange { file: String },
    #[error("overlapping edits detected for file '{file}'")]
    OverlappingEdits { file: String },
    #[error("file '{file}' was not present in the workspace")]
    MissingFile { file: String },
    #[error("patch attempted to create a new file '{file}', but new files are not allowed")]
    NewFileNotAllowed { file: String },
    #[error("file '{file}' already exists in the workspace")]
    FileAlreadyExists { file: String },
    #[error("invalid unified diff: {0}")]
    InvalidUnifiedDiff(String),
    #[error("unified diff did not apply cleanly: {0}")]
    UnifiedDiffApplyFailed(String),
}

#[derive(Debug)]
struct OffsetEdit {
    file: String,
    start: usize,
    end: usize,
    insert: String,
}

fn apply_edits_to_text(
    original: &str,
    edits: Vec<&TextEdit>,
) -> Result<(String, Vec<TextRange>), PatchApplyError> {
    let mut offset_edits = Vec::with_capacity(edits.len());
    let style = TextStyle::from_original(original);
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
            file: edit.file.clone(),
            start,
            end,
            insert: normalize_line_endings(&edit.text, style.line_ending),
        });
    }

    offset_edits.sort_by_key(|edit| edit.start);

    let mut last_end = 0usize;
    for edit in &offset_edits {
        if edit.start < last_end {
            return Err(PatchApplyError::OverlappingEdits {
                file: edit.file.clone(),
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
    let mut touched_spans: Vec<(usize, usize)> = Vec::with_capacity(offset_edits.len());
    for edit in &offset_edits {
        out.push_str(&original[cursor..edit.start]);
        let start_offset = out.len();
        out.push_str(&edit.insert);
        let end_offset = out.len();
        touched_spans.push((start_offset, end_offset));
        cursor = edit.end;
    }
    out.push_str(&original[cursor..]);

    let mut touched_ranges: Vec<TextRange> = Vec::with_capacity(touched_spans.len());
    for (mut start, mut end) in touched_spans {
        if start == end {
            if end < out.len() {
                end = end.saturating_add(1);
            } else if start > 0 {
                start = start.saturating_sub(1);
            }
        }
        touched_ranges.push(TextRange::new(
            text_size_from_usize(start),
            text_size_from_usize(end),
        ));
    }

    Ok((out, touched_ranges))
}

fn apply_text_edits_into(
    workspace: &mut VirtualWorkspace,
    edits: &[TextEdit],
    config: &PatchApplyConfig,
    created_files: &mut BTreeSet<String>,
    deleted_files: &mut BTreeSet<String>,
) -> Result<BTreeMap<String, Vec<TextRange>>, PatchApplyError> {
    let mut files: BTreeMap<String, Vec<&TextEdit>> = BTreeMap::new();
    for edit in edits {
        files.entry(edit.file.clone()).or_default().push(edit);
    }

    let mut touched_ranges: BTreeMap<String, Vec<TextRange>> = BTreeMap::new();
    for (file, file_edits) in files {
        let original = match workspace.files.get(&file) {
            Some(text) => text.as_str(),
            None => {
                if !config.allow_new_files {
                    return Err(PatchApplyError::MissingFile { file: file.clone() });
                }
                created_files.insert(file.clone());
                deleted_files.remove(&file);
                ""
            }
        };
        let (new_text, ranges) = apply_edits_to_text(original, file_edits)?;
        workspace.files.insert(file.clone(), new_text);
        touched_ranges.insert(file, ranges);
    }

    Ok(touched_ranges)
}

fn text_size_to_usize(size: TextSize) -> usize {
    u32::from(size) as usize
}

fn text_size_from_usize(offset: usize) -> TextSize {
    TextSize::from(offset as u32)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineEnding {
    Lf,
    CrLf,
}

impl LineEnding {
    fn as_str(self) -> &'static str {
        match self {
            LineEnding::Lf => "\n",
            LineEnding::CrLf => "\r\n",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TextStyle {
    line_ending: LineEnding,
    trailing_newline: bool,
}

impl TextStyle {
    fn from_original(text: &str) -> Self {
        Self {
            line_ending: detect_line_ending(text),
            trailing_newline: has_trailing_newline(text),
        }
    }
}

fn detect_line_ending(text: &str) -> LineEnding {
    let bytes = text.as_bytes();
    let mut idx = 0usize;
    while idx < bytes.len() {
        match bytes[idx] {
            b'\n' => {
                if idx > 0 && bytes[idx - 1] == b'\r' {
                    return LineEnding::CrLf;
                }
                return LineEnding::Lf;
            }
            b'\r' => {
                if idx + 1 < bytes.len() && bytes[idx + 1] == b'\n' {
                    return LineEnding::CrLf;
                }
                return LineEnding::Lf;
            }
            _ => idx += 1,
        }
    }
    LineEnding::Lf
}

fn has_trailing_newline(text: &str) -> bool {
    text.ends_with('\n') || text.ends_with('\r')
}

fn normalize_line_endings(text: &str, line_ending: LineEnding) -> String {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    match line_ending {
        LineEnding::Lf => normalized,
        LineEnding::CrLf => normalized.replace('\n', "\r\n"),
    }
}

fn join_lines(lines: &[String], style: TextStyle) -> String {
    if lines.is_empty() {
        return String::new();
    }

    let mut out = lines.join(style.line_ending.as_str());
    if style.trailing_newline {
        out.push_str(style.line_ending.as_str());
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnifiedDiffFileOp {
    Create,
    Delete,
    Rename,
    Modify,
}

fn infer_text_style_for_new_file(workspace: &VirtualWorkspace, path: &str) -> TextStyle {
    let target_dir = parent_directory(path);
    let in_same_dir = workspace.files.iter().find(|(candidate, _contents)| {
        // Prefer any existing file that already lives in the same directory as the file being
        // created so we follow that directory's prevailing style (LF/CRLF + trailing newline).
        parent_directory(candidate) == target_dir
    });

    if let Some((_path, contents)) = in_same_dir {
        return TextStyle::from_original(contents);
    }

    // Otherwise fall back to any file in the workspace (deterministic due to BTreeMap ordering).
    if let Some((_path, contents)) = workspace.files.iter().next() {
        return TextStyle::from_original(contents);
    }

    // Empty workspace: fall back to a conventional default.
    TextStyle {
        line_ending: LineEnding::Lf,
        trailing_newline: true,
    }
}

fn parent_directory(path: &str) -> &str {
    // Patch paths are normalized to use `/` separators (see safety.rs); keep the logic simple and
    // platform-independent.
    match path.rsplit_once('/') {
        Some((dir, _file)) => dir,
        None => "",
    }
}

fn classify_unified_diff_file(
    file: &crate::patch::UnifiedDiffFile,
) -> Result<(UnifiedDiffFileOp, String, String), PatchApplyError> {
    if file.old_path == "/dev/null" && file.new_path == "/dev/null" {
        return Err(PatchApplyError::InvalidUnifiedDiff(
            "both old and new paths were /dev/null".into(),
        ));
    }

    if file.old_path == "/dev/null" {
        return Ok((
            UnifiedDiffFileOp::Create,
            file.old_path.clone(),
            file.new_path.clone(),
        ));
    }
    if file.new_path == "/dev/null" {
        return Ok((
            UnifiedDiffFileOp::Delete,
            file.old_path.clone(),
            file.new_path.clone(),
        ));
    }
    if file.old_path != file.new_path {
        return Ok((
            UnifiedDiffFileOp::Rename,
            file.old_path.clone(),
            file.new_path.clone(),
        ));
    }
    Ok((
        UnifiedDiffFileOp::Modify,
        file.old_path.clone(),
        file.new_path.clone(),
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AppliedHunk {
    start_line: usize,
    new_len: usize,
}

const MAX_HUNK_FUZZ_LINES: usize = 20;

fn apply_unified_diff_hunks(
    original: &str,
    hunks: &[UnifiedDiffHunk],
    style: TextStyle,
    file: &str,
) -> Result<(String, Vec<AppliedHunk>), PatchApplyError> {
    let original_lines: Vec<&str> = original.lines().collect();
    let mut output_lines: Vec<String> = Vec::new();
    let mut cursor = 0usize;
    let mut applied = Vec::with_capacity(hunks.len());

    for (hunk_idx, hunk) in hunks.iter().enumerate() {
        let expected_old_start = if hunk.old_start == 0 {
            0usize
        } else {
            hunk.old_start.saturating_sub(1)
        };

        let guess = expected_old_start.max(cursor).min(original_lines.len());
        let start = find_hunk_start(&original_lines, cursor, guess, hunk).map_err(|mismatch| {
            PatchApplyError::UnifiedDiffApplyFailed(format!(
                "file '{file}': failed to apply hunk {hunk_idx} (expected old line {}): {mismatch}",
                expected_old_start + 1
            ))
        })?;

        output_lines.extend(
            original_lines[cursor..start]
                .iter()
                .map(|line| (*line).to_string()),
        );
        let new_start_line = output_lines.len();

        let mut old_index = start;
        for (line_idx, line) in hunk.lines.iter().enumerate() {
            match line {
                UnifiedDiffLine::Context(text) => {
                    let original_line = original_lines.get(old_index).ok_or_else(|| {
                        PatchApplyError::UnifiedDiffApplyFailed(format!(
                            "file '{file}': hunk {hunk_idx} line {line_idx}: context beyond end of file"
                        ))
                    })?;
                    if original_line != text {
                        return Err(PatchApplyError::UnifiedDiffApplyFailed(format!(
                            "file '{file}': hunk {hunk_idx} line {line_idx}: context mismatch at old line {}",
                            old_index + 1,
                        )));
                    }
                    output_lines.push(text.clone());
                    old_index += 1;
                }
                UnifiedDiffLine::Remove(text) => {
                    let original_line = original_lines.get(old_index).ok_or_else(|| {
                        PatchApplyError::UnifiedDiffApplyFailed(format!(
                            "file '{file}': hunk {hunk_idx} line {line_idx}: remove beyond end of file"
                        ))
                    })?;
                    if original_line != text {
                        return Err(PatchApplyError::UnifiedDiffApplyFailed(format!(
                            "file '{file}': hunk {hunk_idx} line {line_idx}: remove mismatch at old line {}",
                            old_index + 1,
                        )));
                    }
                    old_index += 1;
                }
                UnifiedDiffLine::Add(text) => output_lines.push(text.clone()),
            }
        }

        cursor = old_index;
        applied.push(AppliedHunk {
            start_line: new_start_line,
            new_len: hunk.new_len,
        });
    }

    output_lines.extend(
        original_lines[cursor..]
            .iter()
            .map(|line| (*line).to_string()),
    );

    let style = if output_lines.is_empty() {
        TextStyle {
            trailing_newline: false,
            ..style
        }
    } else {
        style
    };

    Ok((join_lines(&output_lines, style), applied))
}

#[derive(Debug)]
enum HunkMismatch {
    MissingLines,
    ContextMismatch { line: usize },
    RemoveMismatch { line: usize },
}

impl std::fmt::Display for HunkMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HunkMismatch::MissingLines => write!(f, "hunk context went past end of file"),
            HunkMismatch::ContextMismatch { line } => write!(f, "context mismatch at old line {line}"),
            HunkMismatch::RemoveMismatch { line } => write!(f, "remove mismatch at old line {line}"),
        }
    }
}

fn find_hunk_start(
    original_lines: &[&str],
    cursor: usize,
    guess: usize,
    hunk: &UnifiedDiffHunk,
) -> Result<usize, String> {
    let old_required = hunk
        .lines
        .iter()
        .filter(|line| {
            matches!(
                line,
                UnifiedDiffLine::Context(_) | UnifiedDiffLine::Remove(_)
            )
        })
        .count();

    if old_required == 0 {
        return Ok(guess);
    }

    if matches_hunk_at(original_lines, guess, hunk).is_ok() {
        return Ok(guess);
    }

    for offset in 1..=MAX_HUNK_FUZZ_LINES {
        if guess >= offset {
            let candidate = guess - offset;
            if candidate >= cursor && matches_hunk_at(original_lines, candidate, hunk).is_ok() {
                return Ok(candidate);
            }
        }

        let candidate = guess + offset;
        if candidate >= cursor && candidate <= original_lines.len() {
            if matches_hunk_at(original_lines, candidate, hunk).is_ok() {
                return Ok(candidate);
            }
        }
    }

    let mismatch = matches_hunk_at(original_lines, guess, hunk).unwrap_err();
    Err(format!(
        "{mismatch}; searched ±{MAX_HUNK_FUZZ_LINES} lines around old line {}",
        guess + 1
    ))
}

fn matches_hunk_at(
    original_lines: &[&str],
    start: usize,
    hunk: &UnifiedDiffHunk,
) -> Result<(), HunkMismatch> {
    let mut old_index = start;
    for line in &hunk.lines {
        match line {
            UnifiedDiffLine::Context(text) => {
                let original = original_lines
                    .get(old_index)
                    .ok_or(HunkMismatch::MissingLines)?;
                if original != text {
                    return Err(HunkMismatch::ContextMismatch { line: old_index + 1 });
                }
                old_index += 1;
            }
            UnifiedDiffLine::Remove(text) => {
                let original = original_lines
                    .get(old_index)
                    .ok_or(HunkMismatch::MissingLines)?;
                if original != text {
                    return Err(HunkMismatch::RemoveMismatch { line: old_index + 1 });
                }
                old_index += 1;
            }
            UnifiedDiffLine::Add(_) => {}
        }
    }
    Ok(())
}

fn approximate_hunk_ranges(text: &str, applied: &[AppliedHunk]) -> Vec<TextRange> {
    if applied.is_empty() {
        return Vec::new();
    }

    let index = LineIndex::new(text);
    let text_len = text.len();
    let line_count = index.line_count();

    applied
        .iter()
        .map(|hunk| {
            let start_line = hunk.start_line as u32;
            let end_line = hunk.start_line.saturating_add(hunk.new_len) as u32;

            let start = index
                .line_start(start_line)
                .map(text_size_to_usize)
                .unwrap_or(text_len);
            let end = if end_line < line_count {
                index
                    .line_start(end_line)
                    .map(text_size_to_usize)
                    .unwrap_or(text_len)
            } else {
                text_len
            };

            let (mut start, mut end) = (start, end);
            if start == end {
                if end < text_len {
                    end = end.saturating_add(1);
                } else if start > 0 {
                    start = start.saturating_sub(1);
                }
            }

            TextRange::new(text_size_from_usize(start), text_size_from_usize(end))
        })
        .collect()
}

pub fn affected_files(patch: &Patch) -> BTreeSet<String> {
    match patch {
        Patch::Json(patch) => {
            let mut out = BTreeSet::new();
            for edit in &patch.edits {
                out.insert(edit.file.clone());
            }
            for op in &patch.ops {
                match op {
                    JsonPatchOp::Create { file, .. } | JsonPatchOp::Delete { file } => {
                        out.insert(file.clone());
                    }
                    JsonPatchOp::Rename { from, to } => {
                        out.insert(from.clone());
                        out.insert(to.clone());
                    }
                }
            }
            out
        }
        Patch::UnifiedDiff(diff) => diff
            .files
            .iter()
            .flat_map(|file| [file.old_path.clone(), file.new_path.clone()])
            .filter(|path| path != "/dev/null")
            .collect(),
    }
}
