class Outer {
  class Inner {}

  <T> Outer(T t) {}

  class Nested {
    void m(Outer o, String s) {
      o.new Inner();
      Outer.this.new Inner();
      o.<String>new Inner();
      Outer.this.<String>new Inner();
      new <String> Outer(s);
    }
  }
}
