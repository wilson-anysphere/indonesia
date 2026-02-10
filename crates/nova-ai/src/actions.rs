use std::borrow::Cow;

use crate::util::markdown::escape_markdown_fence_payload;

fn escape_triple_backticks(text: &str) -> Cow<'_, str> {
    escape_markdown_fence_payload(text)
}

pub fn explain_error_prompt(diagnostic_message: &str, context: &str) -> String {
    let diagnostic_message = escape_triple_backticks(diagnostic_message);
    format!(
        "Explain the following Java compiler error in plain language.\n\n\
         Error:\n```text\n{diagnostic_message}\n```\n\n\
         Code context:\n{context}\n\n\
         Provide:\n\
         1) What the error means\n\
         2) Why it happened\n\
         3) How to fix it\n",
        diagnostic_message = diagnostic_message.as_ref()
    )
}

pub(crate) fn generate_method_body_prompt(method_signature: &str, context: &str) -> String {
    let method_signature = escape_triple_backticks(method_signature);
    format!(
        "Implement the following Java method.\n\n\
         Method signature:\n```java\n{method_signature}\n```\n\n\
         Context:\n{context}\n\n\
         Return ONLY the method body contents (no surrounding braces, no markdown).\n",
        method_signature = method_signature.as_ref()
    )
}

pub(crate) fn generate_tests_prompt(target: &str, context: &str) -> String {
    let target = escape_triple_backticks(target);
    format!(
        "Generate unit tests (JUnit 5) for the following target.\n\n\
         Target:\n```text\n{target}\n```\n\n\
         Context:\n{context}\n\n\
         Include tests for normal cases, edge cases, and error conditions.\n\
         Return ONLY Java code (no markdown).\n",
        target = target.as_ref()
    )
}

pub(crate) fn code_review_prompt(diff: &str) -> String {
    // The code review diff is wrapped in a Markdown fenced code block. If the diff itself contains
    // triple-backtick sequences (common in Markdown files), it can prematurely terminate the fence
    // and confuse both the model and Nova's prompt sanitizer. Break the sequence with zero-width
    // spaces to keep it readable while preventing accidental fence termination.
    let diff = diff.replace("```", "`\u{200B}`\u{200B}`");
    format!(
        r#"Review the following code change.

Notes:
- The diff/context you receive may be incomplete because some files or hunks can be omitted by
  `excluded_paths` privacy filtering. Do not assume missing context; call out limitations when
  relevant.
- The diff may also be truncated to fit context limits. If you notice truncation/omission markers,
  mention that the review is necessarily partial.
- Focus on actionable feedback with concrete, code-referencing suggestions.
- Do not invent file paths, line numbers, or surrounding code that is not present in the diff. If
  something is unclear due to missing context, ask a question in "Questions / Follow-ups".

## Diff
```diff
{diff}
```

Return plain Markdown (no JSON) using this structure:
- Do not wrap the entire response in a single fenced code block (no surrounding Markdown code fences
  around the whole answer). Only use code fences for small code snippets.
- Start your response with the `## Summary` heading.

## Summary
- 1-3 bullets describing what changed and overall risk.
- Always include this heading even if the diff is empty/omitted.

## Issues & Suggestions
- Prefer grouping by file when file paths are available in the diff. Use `### path/to/File.java`
  headings.
- If file names are not available, group by category using: `### Correctness`, `### Performance`,
  `### Security`, `### Tests`, `### Maintainability`.
- List issues in descending severity (`BLOCKER` → `MAJOR` → `MINOR`). If you found no issues, write
  `- None` under this heading.

For each issue/suggestion include:
- **[SEVERITY]** short title (`BLOCKER`, `MAJOR`, or `MINOR`)
- **Where:** file + function/method (or diff hunk) you are referring to
- **Why it matters:** impact/risk
- **Suggestion:** a concrete change (quote exact lines or show a small corrected snippet)

Severity guidance:
- `BLOCKER`: must fix before merge (likely bug/security issue/crash/data loss)
- `MAJOR`: important to address soon (likely correctness/perf/maintainability risk)
- `MINOR`: nice-to-have improvements (style/naming/small refactor)

## Tests
- Missing tests or risky areas + specific test cases to add.
- If no additional tests are needed, write `- None`.

(Optional) ## Positive Notes
(Optional) ## Questions / Follow-ups

If the diff is incomplete/omitted/truncated (e.g. due to `excluded_paths` filtering or diff
truncation), explicitly state that you cannot give complete file-specific feedback and call out
which kinds of issues you may have missed due to the missing context.
"#
    )
}
