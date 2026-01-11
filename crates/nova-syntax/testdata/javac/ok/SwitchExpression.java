class SwitchExpression {
  int f(int x) {
    return switch (x) {
      case 1 -> 10;
      case 2 -> 20;
      default -> {
        yield 0;
      }
    };
  }
}

