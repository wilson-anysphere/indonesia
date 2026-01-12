class C {
    int sum(int a, int b) {
        return extracted(a, b);
    }

    private int extracted(int a, int b) {
        return a + b;
    }
}

