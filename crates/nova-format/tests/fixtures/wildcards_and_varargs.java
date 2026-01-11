import java.util.*;

class WildcardsAndVarargs{
List<? extends Number>nums;
List<?super String>strings;
Map<String,? extends List<Integer>>map;

void m(String...args){
}
}

