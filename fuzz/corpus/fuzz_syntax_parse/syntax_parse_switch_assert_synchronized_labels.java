class Foo { void m(int x) { label: synchronized (this) { assert true; } switch (x) { case 1: break; default: break; case 2 -> { return; } } } }
