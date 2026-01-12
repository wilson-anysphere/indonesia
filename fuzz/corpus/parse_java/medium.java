package bench;

import java.util.ArrayList;
import java.util.List;

public class Medium {
  private final List<String> names = new ArrayList<>();

  public Medium() {
    names.add("alpha");
    names.add("beta");
    names.add("gamma");
  }

  public int sumLengths() {
    int total = 0;
    for (String name : names) {
      total += name.length();
    }
    return total;
  }

  public void sortAndPrint() {
    names.sort(String::compareTo);
    for (String name : names) {
      System.out.println(name.toUpperCase());
    }
  }

  public String compute(int seed) {
    StringBuilder builder = new StringBuilder();
    for (int i = 0; i < 100; i++) {
      builder.append(seed + i).append(':');
    }
    return builder.toString();
  }
}

