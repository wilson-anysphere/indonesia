import java.util.ArrayList;
import java.util.List;

class Generics {
  List<String> xs = new ArrayList<>();

  String first() {
    return xs.get(0);
  }
}

