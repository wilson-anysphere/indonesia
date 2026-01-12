rootProject.name = "gradle-projectdir-mapping-kts"

include(
    ":app",
    ":lib",
)

// Override module locations to non-standard dirs (Kotlin DSL).
project(":app").projectDir = file("modules/app")
project(":lib").projectDir = java.io.File(settingsDir, "modules/lib")

