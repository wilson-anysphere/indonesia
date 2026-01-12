record Point(int x, int y) {
  Point {
    if (x < 0 || y < 0) {
      throw new IllegalArgumentException();
    }
  }
}
