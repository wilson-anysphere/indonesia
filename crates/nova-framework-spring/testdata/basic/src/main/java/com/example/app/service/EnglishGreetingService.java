package com.example.app.service;

import org.springframework.context.annotation.Primary;
import org.springframework.stereotype.Service;

@Service
@Primary
public class EnglishGreetingService implements GreetingService {
    @Override
    public String greet() {
        return "hi";
    }
}

