package com.example.app;

import com.example.greeting.Greeter;
import com.google.gson.Gson;

/**
 * The main class of them all
 * If it would not be main it would be minor
 */

public class Main {
    public static void main(String[] args) {
        Greeter greeter = new Greeter("world");
        Gson gson = new Gson();

        System.out.println(greeter.greet() + " -> " + gson.toJson(greeter));
    }
}
