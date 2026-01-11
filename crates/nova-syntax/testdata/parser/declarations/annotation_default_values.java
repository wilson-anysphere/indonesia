@interface A {
  int value() default 1;
  String[] names() default {"a", "b"};
  B ann() default @B(x = 1);
  int other();
}
