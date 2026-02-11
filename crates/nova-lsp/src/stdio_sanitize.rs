pub(crate) fn sanitize_serde_json_error(err: &serde_json::Error) -> String {
    // Reuse the sanitizer in the `nova_lsp` library so the bin target doesn't
    // duplicate redaction logic (bin unit tests are disabled to keep link-time
    // memory usage down).
    nova_lsp::sanitize_error_message(err)
}
