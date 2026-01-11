class Foo {
  int m(int x) {
    return switch (x) {
      case 1 -> 10;
      default -> 0;
    };
  }
}
