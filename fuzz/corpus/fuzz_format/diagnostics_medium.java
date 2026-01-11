package bench;

import java.util.ArrayList;
import java.util.List;

public class DiagnosticsMedium {
  private final List<String> names = new ArrayList<>();

  public DiagnosticsMedium() {
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

  public void broken() {
    // Intentional syntax error to keep parse-error traversal in the benchmark.
    int x = 1
  }

  public void longLine() {
    // The next line is intentionally long to keep the simple line-scanning lint in play.
    String s = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    System.out.println(s);
  }
}

