package com.example.greeting;

public class Greeter {
    private final String name;

    public static class Inner {
        public static final String CONST = "const";

        private int val;

        public Inner(int val) {
            this.val = val;
        }

        public int getVal() {
            return this.val;
        }
    }

    public Greeter(String name) {
        this.name = name;
    }

    public String getName() {
        return name;
    }

    public String greet() {
        return "Hello, " + name + "!";
    }
}
