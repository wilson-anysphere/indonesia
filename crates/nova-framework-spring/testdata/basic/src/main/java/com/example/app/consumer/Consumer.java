package com.example.app.consumer;

import com.example.app.foo.Foo;
import com.example.app.service.GreetingService;
import com.other.OtherService;
import org.springframework.beans.factory.annotation.Autowired;
import org.springframework.beans.factory.annotation.Qualifier;
import org.springframework.beans.factory.annotation.Value;
import org.springframework.stereotype.Component;

@Component
public class Consumer {
    @Autowired
    Foo foo;

    @Autowired
    OtherService otherService;

    @Value("${server.p}")
    String port;

    @Autowired
    public Consumer(GreetingService greetingService, @Qualifier("spanish") GreetingService spanish) {
    }
}

