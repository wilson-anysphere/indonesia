class Foo {
  <T> T id(T t) { return t; }

  void m(String s) {
    <String>id(s);
  }
}
