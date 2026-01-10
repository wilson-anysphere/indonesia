package test;

import dagger.Module;
import dagger.Provides;

@Module
class FooModule {
  @Provides
  Foo provideFoo1() {
    return new Foo();
  }

  @Provides
  Foo provideFoo2() {
    return new Foo();
  }
}

