package com.example;

import org.junit.jupiter.api.*;
import static org.junit.jupiter.api.Assertions.*;

public class WildcardImportTest {
    @Test
    public void works() {
        assertTrue(true);
    }

    @RepeatedTest(2)
    public void repeats() {
        // no-op
    }

    @TestFactory
    public void factory() {
        // no-op
    }

    @TestTemplate
    public void template() {
        // no-op
    }
}

