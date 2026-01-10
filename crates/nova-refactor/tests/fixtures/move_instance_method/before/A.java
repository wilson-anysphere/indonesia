public class A {
    public B b = new B();
    int base = 10;

    public int compute(int x) {
        return base + b.inc(x);
    }
}

