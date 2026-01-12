plugins {
    java
}

dependencies {
    // Kotlin DSL forms.
    implementation(files("libs/local.jar"))
    implementation(fileTree("libs") { include("*.jar") })
}
