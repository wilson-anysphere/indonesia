package com.example.dep;

import java.util.List;

public class Foo {
    public List<Integer> nums;

    public List<String> strings() {
        return null;
    }

    public <T> T id(T value) {
        return value;
    }

    public static class Inner {
        public String value() {
            return "";
        }
    }
}

