plugins {
    id("java")
}

repositories {
    mavenCentral()
}

dependencies {
    implementation(libs.guava)
    implementation(libs.slf4j.api.get())
    implementation(libs.no.version)
    implementation(libs.strict.lib)
    testImplementation(libs.bundles.test.libs)
}
