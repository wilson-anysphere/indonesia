package com.example;

import org.junit.jupiter.api.Test;

class TypeDeclarationContainer {
    record NestedRecord() {
        @Test
        void worksInNestedRecord() {
            // no-op
        }
    }

    enum NestedEnum {
        VALUE;

        @Test
        void worksInNestedEnum() {
            // no-op
        }
    }
}

