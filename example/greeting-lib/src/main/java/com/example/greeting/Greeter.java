package com.example.greeting;

public class Greeter {
    private final String name;

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
