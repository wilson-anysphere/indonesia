class Foo {
  int m(int x) {
    return switch (x) {
      default -> { yield 0 }
    };
  }
}
class Bar {}
