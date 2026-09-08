package com.example.greeting;

import static org.junit.Assert.assertEquals;

import org.junit.Test;

public class GreeterTest {
    @Test
    public void greets_by_name() {
        assertEquals("Hello, Ada!", new Greeter("Ada").greet());
    }
}
