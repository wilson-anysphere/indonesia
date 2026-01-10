package com.example;

import org.junit.jupiter.api.Test;
import org.junit.jupiter.params.ParameterizedTest;
import org.junit.jupiter.params.provider.ValueSource;

public class CalculatorTest {
    @Test
    public void adds() {
        // no-op
    }

    @ParameterizedTest
    @ValueSource(ints = {1, 2, 3})
    public void parameterizedAdds(int value) {
        // no-op
    }
}

