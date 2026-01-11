class InstanceofPattern {
  static int len(Object o) {
    if (o instanceof String s) {
      return s.length();
    }
    return 0;
  }
}

