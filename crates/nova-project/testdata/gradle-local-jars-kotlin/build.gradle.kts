plugins {
    java
}

dependencies {
    // Kotlin DSL forms.
    implementation(files("libs/local.jar"))
    implementation(fileTree("libs") { include("*.jar") })
    implementation(fileTree(mapOf("dir" to "other-libs", "include" to listOf("*.jar"))))
    implementation(fileTree(dir = file("file-libs")) { include("*.jar") })
}
