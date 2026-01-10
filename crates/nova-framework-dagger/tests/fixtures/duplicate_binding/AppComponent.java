package test;

import dagger.Component;

@Component(modules = FooModule.class)
interface AppComponent {
  Consumer consumer();
}

