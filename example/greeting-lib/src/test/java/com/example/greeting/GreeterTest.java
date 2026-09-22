package com.example.greeting;

import org.junit.Test;


import static org.junit.Assert.assertEquals;

public class GreeterTest {
    @Test
    public void greets_by_name() {
        assertEquals("Hello, Ada!", new Greeter("Ada").greet());
    }
}

