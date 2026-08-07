buildscript {
    repositories { mavenCentral() }
    dependencies {
        // ObjectBox applies via a classpath plugin (not the plugins DSL).
        classpath("io.objectbox:objectbox-gradle-plugin:4.0.3")
    }
}

plugins {
    id("com.android.application") version "8.11.1" apply false
    id("org.jetbrains.kotlin.android") version "2.0.21" apply false
    id("com.google.devtools.ksp") version "2.0.21-1.0.28" apply false
    id("io.realm.kotlin") version "3.0.0" apply false
    id("app.cash.sqldelight") version "2.0.2" apply false
}
