class SwitchArrow {
  String label(int x) {
    switch (x) {
      case 1 -> {
        return "one";
      }
      case 2 -> return "two";
      default -> return "many";
    }
  }
}

