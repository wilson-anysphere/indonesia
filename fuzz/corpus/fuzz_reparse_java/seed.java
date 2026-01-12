class Foo {
  void m() {
    int x = 1;
  }
}

class Bar {}

// The fuzz target splits the input into (old_text, replacement) and performs one edit.
// This seed gives libFuzzer something Java-ish to start from.

