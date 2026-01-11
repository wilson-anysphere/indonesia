class Foo {
  Object f = new Object() {
    int x;

    @Override
    public String toString() {
      return "ok";
    }
  };
}
