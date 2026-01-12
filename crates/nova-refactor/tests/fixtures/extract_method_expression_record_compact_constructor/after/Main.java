record R(int x) {
    R {
        int y = extracted(x);
        System.out.println(y);
    }

    private int extracted(int x) {
        return x + 1;
    }
}

