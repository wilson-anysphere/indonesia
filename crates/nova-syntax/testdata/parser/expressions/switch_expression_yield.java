class Foo {
  int m(int x) {
    return switch (x) {
      case 1 -> 1;
      default -> { yield 0; }
    };
  }
}
