record Point(int x, int y) {
    Point {
        extracted(x, y);
    }

    private void extracted(int x, int y) {
        System.out.println(x + y);
    }
}

