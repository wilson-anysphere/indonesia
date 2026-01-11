package com.example.app.service;

import org.springframework.beans.factory.annotation.Qualifier;
import org.springframework.stereotype.Service;

@Service
@Qualifier("spanish")
public class SpanishGreetingService implements GreetingService {
    @Override
    public String greet() {
        return "hola";
    }
}

