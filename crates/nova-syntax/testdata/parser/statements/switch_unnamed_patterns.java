class Foo {
  void m(Object o) {
    switch (o) {
      case String _ -> {}
      case Point(int _, int y) -> {}
      case Point(_, _) -> {}
      default -> {}
    }
  }
}
