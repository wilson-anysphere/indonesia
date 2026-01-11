class Foo {
  void m() {
    try (var x = open(); y) {
      throw new RuntimeException();
    } catch (IOException | RuntimeException e) {
      return;
    } finally {
      assert true;
    }
  }
}
