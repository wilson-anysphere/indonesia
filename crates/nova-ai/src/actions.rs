pub(crate) fn explain_error_prompt(diagnostic_message: &str, context: &str) -> String {
    format!(
        "Explain the following Java compiler error in plain language.\n\n\
         Error:\n```text\n{diagnostic_message}\n```\n\n\
         Code context:\n{context}\n\n\
         Provide:\n\
         1) What the error means\n\
         2) Why it happened\n\
         3) How to fix it\n"
    )
}

pub(crate) fn generate_method_body_prompt(method_signature: &str, context: &str) -> String {
    format!(
        "Implement the following Java method.\n\n\
         Method signature:\n```java\n{method_signature}\n```\n\n\
         Context:\n{context}\n\n\
         Return ONLY the method body contents (no surrounding braces, no markdown).\n"
    )
}

pub(crate) fn generate_tests_prompt(target: &str, context: &str) -> String {
    format!(
        "Generate unit tests (JUnit 5) for the following target.\n\n\
         Target:\n```text\n{target}\n```\n\n\
         Context:\n{context}\n\n\
         Include tests for normal cases, edge cases, and error conditions.\n\
         Return ONLY Java code (no markdown).\n"
    )
}

pub(crate) fn code_review_prompt(diff: &str) -> String {
    format!(
        r#"Review the following code change.

Notes:
- The diff/context you receive may be incomplete because some files or hunks can be omitted by
  `excluded_paths` privacy filtering. Do not assume missing context; call out limitations when
  relevant.
- The diff may also be truncated to fit context limits. If you notice truncation/omission markers,
  mention that the review is necessarily partial.
- Focus on actionable feedback with concrete, code-referencing suggestions.

## Diff
```diff
{diff}
```

Return plain Markdown (no JSON) using this structure:

## Summary
- 1-3 bullets describing what changed and overall risk.

## Issues & Suggestions
- Prefer grouping by file when file paths are available in the diff. Use `### path/to/File.java`
  headings.
- If file names are not available, group by category using: `### Correctness`, `### Performance`,
  `### Security`, `### Tests`, `### Maintainability`.

For each issue/suggestion include:
- **[SEVERITY]** short title (`BLOCKER`, `MAJOR`, or `MINOR`)
- **Where:** file + function/method (or diff hunk) you are referring to
- **Why it matters:** impact/risk
- **Suggestion:** a concrete change (quote exact lines or show a small corrected snippet)

## Tests
- Missing tests or risky areas + specific test cases to add.

(Optional) ## Positive Notes
(Optional) ## Questions / Follow-ups

If the diff is missing/omitted (e.g. the diff section contains an omission placeholder due to
`excluded_paths`), explicitly state that you cannot give file-specific feedback and provide only
general review guidance.
"#
    )
}
