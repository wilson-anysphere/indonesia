@Deprecated
open module com.example.mod {
  requires transitive java.base;
  requires static java.sql;
  exports com.example.api;
  exports com.example.internal to java.base, java.logging;
  opens com.example.internal to java.base;
  uses com.example.spi.Service;
  provides com.example.spi.Service with com.example.impl.ServiceImpl, com.example.impl.OtherImpl;
}

