package com.example.app;

import com.example.greeting.Greeter;
import com.google.gson.Gson;
import com.example.greeting.Data;
import org.apache.commons.lang3.StringUtils;
import com.example.greeting.DataType;
import one.example.com.SumType;
import one.example.com.SumType.Type1;

import java.util.List;

/**
 * The main class of them all
 * If it would not be main it would be minor
 */
public class Main {
    public static void main(String[] args) {
        Greeter greeter = new Greeter("world");
        Gson gson = new Gson();
        var data = new Data(5);

        var instance = DataType.TYPE_1;

        var inner = new Greeter.Inner();
        inner.getVal();

        var sumType = new SumType.Type2("test");

        if (sumType instanceof Type1(var val)) {
            System.out.println("Some val" + val);
        }

        data.shout();
        data.test(5);
        data.test(5.0f);

        var stripped = StringUtils.strip(" bla ");

        var name = greeter.getName();
        var greeting = greeter.greet();

        var test = gson.toString();

        System.out.println(greeter.greet() + " -> " + gson.toJson(greeter));
    }
}
