package test;

import javax.inject.Inject;
import javax.inject.Named;

class Foo {
  @Inject
  Foo(@Named("😀") Bar bar) {}
}
