class Misindented {
void m() {
if (true) {
System.out.println("x");
}

foo(1,
2, 3);
}

void foo(int a, int b, int c) {}
}

