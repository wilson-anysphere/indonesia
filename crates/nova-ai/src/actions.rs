pub(crate) fn explain_error_prompt(diagnostic_message: &str, context: &str) -> String {
    format!(
        "Explain the following Java compiler error in plain language.\n\n\
         Error:\n{diagnostic_message}\n\n\
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
         Method signature:\n{method_signature}\n\n\
         Context:\n{context}\n\n\
         Return ONLY the method body contents (no surrounding braces, no markdown).\n"
    )
}

pub(crate) fn generate_tests_prompt(target: &str, context: &str) -> String {
    format!(
        "Generate unit tests (JUnit 5) for the following target.\n\n\
         Target:\n{target}\n\n\
         Context:\n{context}\n\n\
         Include tests for normal cases, edge cases, and error conditions.\n\
         Return ONLY Java code (no markdown).\n"
    )
}

pub(crate) fn code_review_prompt(diff: &str) -> String {
    format!(
        "Review this code change:\n\n{diff}\n\n\
         Consider correctness, performance, security, maintainability, and tests.\n"
    )
}
