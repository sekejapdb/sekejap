plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("com.google.devtools.ksp")
    id("io.realm.kotlin")
    id("app.cash.sqldelight")
}

android {
    namespace = "life.sekejap.bench"
    compileSdk = 35

    defaultConfig {
        applicationId = "life.sekejap.bench"
        minSdk = 26
        targetSdk = 34
        versionCode = 1
        versionName = "1.0"
        ndk { abiFilters += "arm64-v8a" }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            signingConfig = signingConfigs.getByName("debug")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions { jvmTarget = "17" }
}

sqldelight {
    databases {
        create("BenchDb") {
            packageName.set("life.sekejap.bench.sqld")
        }
    }
}

dependencies {
    implementation("androidx.core:core-ktx:1.13.1")
    implementation("androidx.appcompat:appcompat:1.7.0")

    // Room (SQLite)
    implementation("androidx.room:room-runtime:2.7.1")
    implementation("androidx.room:room-ktx:2.7.1")
    ksp("androidx.room:room-compiler:2.7.1")

    // ObjectBox (plugin applied at the bottom)
    implementation("io.objectbox:objectbox-android:4.0.3")
    implementation("io.objectbox:objectbox-kotlin:4.0.3")

    // Realm-Kotlin
    implementation("io.realm.kotlin:library-base:3.0.0")

    // SQLDelight
    implementation("app.cash.sqldelight:android-driver:2.0.2")

    // sekejap ergonomic typed layer (composite build) + its KSP processor.
    // Composite substitution matches the Gradle project name (":processor");
    // real consumers use the published coordinate life.sekejap:sekejap-processor.
    implementation("life.sekejap:sekejap")
    ksp("life.sekejap:processor")
}

// ObjectBox must be applied after the Android/Kotlin plugins and dependencies.
apply(plugin = "io.objectbox")
