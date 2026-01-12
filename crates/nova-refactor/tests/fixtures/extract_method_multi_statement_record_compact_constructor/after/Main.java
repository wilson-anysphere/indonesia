record R(int x) {
    R {
        int y = 1;
        extracted(x, y);
    }

    private void extracted(int x, int y) {
        System.out.println(x);
        System.out.println(y);
    }
}

