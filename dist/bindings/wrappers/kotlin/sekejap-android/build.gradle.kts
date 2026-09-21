import com.vanniktech.maven.publish.AndroidSingleVariantLibrary
import com.vanniktech.maven.publish.SonatypeHost

// The Android distribution: re-exports the typed sekejap API (`:` = sekejap-orm)
// and bundles the JNI `.so` for each Android ABI, so app consumers just add one
// dependency + the KSP processor — no manual jniLibs, no Rust toolchain.
//
// Populate jniLibs first:  JNILIBS=android/jniLibs ../build-native.sh android

plugins {
    id("com.android.library")
    id("org.jetbrains.kotlin.android")
    id("com.vanniktech.maven.publish")
}

group = "life.sekejap"
// Single source of truth: the workspace version in the root Cargo.toml.
version = rootDir.resolve("../../../Cargo.toml").readLines()
    .first { it.trimStart().startsWith("version = ") }
    .substringAfter('"').substringBefore('"')

android {
    namespace = "life.sekejap.android"
    compileSdk = 35
    defaultConfig { minSdk = 24 }
    // Prebuilt JNI libraries (arm64-v8a, armeabi-v7a, x86_64).
    sourceSets["main"].jniLibs.srcDirs("jniLibs")
    // vanniktech's AndroidSingleVariantLibrary configures the release variant.
}

dependencies {
    api("life.sekejap:sekejap:$version") // the typed runtime + API
}

mavenPublishing {
    configure(AndroidSingleVariantLibrary("release", sourcesJar = true, publishJavadocJar = false))
    publishToMavenCentral(SonatypeHost.CENTRAL_PORTAL, automaticRelease = true)
    if (project.findProperty("signingInMemoryKey") != null) signAllPublications()
    coordinates("life.sekejap", "sekejap-android", version.toString())
    pom {
        name.set("sekejap-android")
        description.set("sekejap for Android — the typed reactive API plus bundled JNI native libraries.")
        url.set("https://sekejap.life")
        licenses {
            license { name.set("Apache-2.0"); url.set("https://www.apache.org/licenses/LICENSE-2.0") }
            license { name.set("MIT"); url.set("https://opensource.org/licenses/MIT") }
        }
        developers { developer { id.set("sekejapdb"); name.set("sekejap"); url.set("https://github.com/sekejapdb") } }
        scm {
            url.set("https://github.com/sekejapdb/sekejap")
            connection.set("scm:git:https://github.com/sekejapdb/sekejap.git")
        }
    }
}
