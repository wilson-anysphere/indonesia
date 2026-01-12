// Mixed Java snippets intended to exercise completion on malformed / incomplete code.

module com.example.app {
  requires java.base;
  requires java.sql;
  exports com.example;
}

package com.example;

import java.util.*;
import java.util.stream.*;
import static java.util.stream.Collectors.*;
import java.util.concu

@Deprecated
@interface Ann {
  String value() default "";
}

@Ann("x")
public class Main {
  @SuppressWarnings({"unchecked",})
  private List<String> list = Arrays.asList("a", "b");

  public void run() {
    // chained calls with an incomplete member access
    list.stream()
        .map(s -> s.toUpperCase())
        .collect(toList())
        .get(0).
        ;

    // incomplete generic + constructor
    Map<String, Integer> m = new HashMap<>().

    // incomplete type / identifier in call
    Optional.ofNullable(null).orElseGet(() -> new StringBui);

    // incomplete annotation usage
    @Override publi
  }
}

