class Outer {
  class Inner {
    void m() {
      Outer.this.toString();
      Outer.super.toString();
    }
  }
}
