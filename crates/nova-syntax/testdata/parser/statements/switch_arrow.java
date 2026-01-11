class Foo {
  void m(int x) {
    switch (x) {
      case 1 -> { return; }
      case 2 -> foo();
      default -> { }
    }
  }
  void foo() {}
}
