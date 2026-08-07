pluginManagement {
    repositories {
        google()
        mavenCentral()
        gradlePluginPortal()
    }
    // Central plugin versions (multi-module: declare once, apply without version).
    plugins {
        id("org.jetbrains.kotlin.jvm") version "2.0.21"
        id("org.jetbrains.kotlin.android") version "2.0.21"
        id("com.google.devtools.ksp") version "2.0.21-1.0.28"
        id("com.android.library") version "8.11.1"
        id("com.vanniktech.maven.publish") version "0.30.0"
    }
}
dependencyResolutionManagement {
    repositories {
        google()
        mavenCentral()
    }
}
rootProject.name = "sekejap"
include(":processor")
// NOTE: the Android AAR (:android) lives as a SEPARATE Gradle build — mixing a
// kotlin("jvm") root with a com.android.library submodule breaks the Kotlin
// plugin classpath. It depends on the published life.sekejap:sekejap-orm.
