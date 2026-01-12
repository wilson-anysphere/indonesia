// Framework-heavy snippet to exercise completion branches for Spring/Micronaut/Quarkus/JPA/Dagger.
package com.example;

import java.util.*;

import org.springframework.beans.factory.annotation.Value;
import org.springframework.stereotype.Component;

import io.micronaut.context.annotation.Requires;

import org.eclipse.microprofile.config.inject.ConfigProperty;

import jakarta.persistence.Entity;
import jakarta.persistence.Id;

import javax.inject.Inject;

import org.springframework.data.jpa.repository.Query;
import org.springframework.data.repository.Repository;

@Entity
class User {
  @Id Long id;
  String name;
}

interface UserRepo extends Repository<User, Long> {
  @Query("select u from User u where u.")
  List<User> find();
}

@Component
@Requires(property = "ser")
class Main {
  @Value("${server.p}")
  String port;

  @io.micronaut.context.annotation.Value("${micronaut.ser")
  String micronautPort;

  @ConfigProperty(name = "quarkus.http.po")
  String quarkusPort;

  @Inject
  UserRepo repo;

  void run() {
    repo.find().get(0).getN
  }
}

