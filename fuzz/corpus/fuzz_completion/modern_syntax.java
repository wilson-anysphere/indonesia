// Modern Java syntax + incomplete member access for completion.
package com.example;

public record Person(String name, int age) {
  public Person {
    this.name = name == null ? "" : name;
  }
}

sealed interface Shape permits Circle, Rect {}
final class Circle implements Shape { double r; }
final class Rect implements Shape { int w, h; }

class Test {
  void m(Object o) {
    switch (o) {
      case String s -> s.
      case Integer i -> { yield i.toString(); }
      default -> {}
    }

    var text = """
      hello
      world
      """;
  }
}

