interface Greeter {
  default String greet(String name) {
    return "Hello " + name;
  }
}

