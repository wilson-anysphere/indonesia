class Foo {
  <T> T id(T t) { return t; }

  void m(String s) {
    this.<String>id(s);
    Foo.<String>id(s);
  }
}
