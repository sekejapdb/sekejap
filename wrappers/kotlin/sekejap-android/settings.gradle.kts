pluginManagement {
    repositories { google(); mavenCentral(); gradlePluginPortal() }
    plugins {
        id("org.jetbrains.kotlin.android") version "2.0.21"
        id("com.android.library") version "8.11.1"
        id("com.vanniktech.maven.publish") version "0.30.0"
    }
}
dependencyResolutionManagement {
    repositories { google(); mavenCentral(); mavenLocal() }
}
rootProject.name = "sekejap-android"
