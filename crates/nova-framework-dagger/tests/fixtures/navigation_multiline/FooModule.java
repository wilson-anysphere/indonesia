package test;

import dagger.Module;
import dagger.Provides;

@Module
class FooModule {
  @Provides
  Foo provideFoo(
  ) {
    return new Foo();
  }
}

