class Foo {
  java.util.List<@A String> xs;
  int @B [] ys;

  Object m(Object x) {
    return (@C String) x;
  }

  Foo n() throws @D Exception {
    return this;
  }
}
