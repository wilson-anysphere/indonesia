import java.util.function.Function;

class Lambda {
  int inc(int x) {
    Function<Integer, Integer> f = (Integer n) -> n + 1;
    return f.apply(x);
  }
}

