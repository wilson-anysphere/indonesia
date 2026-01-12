class Base {
  Base() {}
  <T> Base(T t) {}
}

class Foo extends Base {
  <T> Foo(T t) { super(t); }

  Foo() { <String>this("x"); }

  Foo(long x) { this(); }

  Foo(double d) { <String>super(d); }

  class Inner extends Base {
    Inner(Foo f, String s) { f.<String>super(s); }
  }
}
