class Foo<T> {
  Foo() {}
  static void bar() {}
  void m() {
    var r = Foo<String>::bar;
    var c = Foo<String>::new;
    var a = java.util.ArrayList<String>::new;
    var e = java.util.Map.Entry<String, String>::getKey;
  }
}
