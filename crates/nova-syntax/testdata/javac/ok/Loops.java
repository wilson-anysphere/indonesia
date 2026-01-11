class Loops {
  int sum(int[] xs) {
    int s = 0;
    for (int x : xs) {
      s += x;
    }

    int i = 0;
    while (i < 3) {
      i++;
    }

    do {
      i--;
    } while (i > 0);

    return s;
  }
}

