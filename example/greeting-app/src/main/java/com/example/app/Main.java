package com.example.app;

import com.example.greeting.Greeter;
import com.google.gson.Gson;
import com.example.greeting.Data;

/**
 * The main class of them all
 * If it would not be main it would be minor
 */

public class Main {
    public static void main(String[] args) {
        Greeter greeter = new Greeter("world");
        Gson gson = new Gson();
        var data = new Data(5);

        data.shout();
        data.test(5);
        data.test(5.0f);

        var name = greeter.getName();
        var greeting = greeter.greet();

        var test = gson.toString();

        System.out.println(greeter.greet() + " -> " + gson.toJson(greeter));
    }
}

