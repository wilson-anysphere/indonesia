record Point(int x, int y) {
  Point throws java.io.IOException {
    if (x < 0) {
      throw new RuntimeException();
    }
  }
}
