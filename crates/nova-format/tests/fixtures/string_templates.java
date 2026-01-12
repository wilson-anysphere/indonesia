class StringTemplates {
    void demo(String name) {
        String a = STR."Hello \{name}";

        String b = STR."""
            Hello \{name}
                Indented line
            """;

        String c = STR."\\{not_interp}";

        // Single-token template text that looks like punctuation.
        String semi = STR.";";
        String rbrace = STR."}";
        String lbrace = STR."{";

        // A multi-part template where the text segment between interpolations is exactly `}`.
        int x = 1;
        int y = 2;
        String between = STR."\{x}}\{y}";

        // Template text that equals a keyword.
        String keyword = STR."for";

        String d = STR."Lambda: \{() -> { return 1; }} done";

        String e = STR."Nested: \{STR."Inner \{name}"}";
    }
}
