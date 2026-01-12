class C {
    void m(int a, int b) {
        extracted(a, b);
        System.out.println("done");
    }

    private void extracted(int a, int b) {
        System.out.println(a);
        System.out.println(b);
    }
}

