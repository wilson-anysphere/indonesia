open module corpus.ok {
  requires transitive java.logging;
  requires java.sql;
  exports foo;
  uses java.sql.Driver;
}
