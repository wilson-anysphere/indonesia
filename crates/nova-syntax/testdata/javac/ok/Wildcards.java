import java.util.List;

class Wildcards {
  List<?> any;
  List<? extends Number> nums;
  List<? super Integer> ints;
}

