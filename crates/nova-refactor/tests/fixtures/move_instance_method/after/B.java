public class B {
    public int inc(int x) {
        return x + 1;
    }

    public int compute(A a, int x) {
        return a.base + this.inc(x);
    }
}

