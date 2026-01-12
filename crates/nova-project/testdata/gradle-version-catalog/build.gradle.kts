plugins {
    id("java")
}

repositories {
    mavenCentral()
}

dependencies {
    implementation(libs.guava)
    implementation(libs.slf4j.api.get())
    testImplementation(libs.bundles.test.libs)
}

