class StringTemplates {
    void demo(String name) {
        String a = STR."Hello \{name}";

        String b = STR."""
            Hello \{name}
                Indented line
            """;

        String c = STR."\\{not_interp}";
    }
}
