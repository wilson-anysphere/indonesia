module com.example.lib {
    requires com.example.extra;
    requires transitive com.example.util;

    exports com.example.lib.api to com.example.app;
}

