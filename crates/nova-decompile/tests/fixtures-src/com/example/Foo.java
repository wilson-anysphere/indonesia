package com.example;

@Deprecated
public class Foo implements Runnable {
    public static final int ANSWER = 42;
    private final String name;

    public Foo(String name) {
        this.name = name;
    }

    public String getName() {
        return name;
    }

    public int add(int a) {
        return a + 1;
    }

    public int add(int a, int b) {
        return a + b;
    }

    public String[] echo(String[] values) {
        return values;
    }

    @Deprecated
    public void oldMethod() {
    }

    @Override
    public void run() {
    }
}

