// sekejap for the JVM -- one Maven artifact, usable from Java and from Kotlin.
//
// The binding is Panama/FFM (java.lang.foreign, final in JDK 22) over the C ABI
// `libsekejap` (dist/ffi/include/sekejap.h, contract docs/dist/C_ABI.md). Pure
// JVM: no JNI shim to compile, no extra runtime dependency.
//
// Tests and `run` link against a libsekejap built elsewhere. Point at the
// directory that holds it:
//
//   ./gradlew test -Psekejap.nativeDir=/path/to/libsekejap
//   ./gradlew run  -Psekejap.nativeDir=/path/to/libsekejap
//
// or set SEKEJAP_NATIVE_DIR. With neither, the build looks in the workspace's
// own target/release, which is where `cargo build --release -p sekejap-capi`
// leaves it.

import com.vanniktech.maven.publish.JavadocJar
import com.vanniktech.maven.publish.KotlinJvm
import com.vanniktech.maven.publish.SourcesJar
import org.jetbrains.kotlin.gradle.dsl.JvmTarget

plugins {
    kotlin("jvm") version "2.3.21"
    application
    id("com.vanniktech.maven.publish") version "0.37.0"
}

group = "life.sekejap"

// Single source of truth: [workspace.package].version in the root Cargo.toml, so
// a Cargo bump is the ONLY place the version lives.
version = rootDir.resolve("../../../../Cargo.toml").readLines()
    .first { it.trimStart().startsWith("version = ") }
    .substringAfter('"').substringBefore('"')

repositories { mavenCentral() }

dependencies {
    testImplementation(kotlin("test"))
}

// JDK 22 is the floor: the Foreign Function & Memory API was finalized there.
// The build itself runs on any JDK from 22 up; `--release` keeps the bytecode
// and the API surface at 22 whichever one that is.
kotlin {
    compilerOptions {
        jvmTarget.set(JvmTarget.JVM_22)
        freeCompilerArgs.add("-Xjdk-release=22")
    }
}
tasks.withType<JavaCompile>().configureEach {
    options.release.set(22)
}

// The directory that holds libsekejap.{dylib,so,dll} for tests and for `run`.
val nativeDir: String =
    (findProperty("sekejap.nativeDir") as String?)
        ?: System.getenv("SEKEJAP_NATIVE_DIR")
        ?: rootDir.resolve("../../../../target/release").absolutePath

// `--enable-native-access`: FFM downcalls are restricted methods, and naming the
// module here is what keeps the runtime from warning on every call.
val ffmArgs = listOf("--enable-native-access=ALL-UNNAMED", "-Djava.library.path=$nativeDir")

tasks.test {
    useJUnitPlatform()
    jvmArgs(ffmArgs)
    testLogging {
        events("passed", "failed", "skipped")
        showStandardStreams = true
        exceptionFormat = org.gradle.api.tasks.testing.logging.TestExceptionFormat.FULL
    }
}

// `./gradlew run` -- the end-to-end tour in src/main/kotlin/life/sekejap/Example.kt.
application {
    mainClass.set("life.sekejap.ExampleKt")
    applicationDefaultJvmArgs = ffmArgs
}
tasks.named<JavaExec>("run") {
    jvmArgs(ffmArgs)
}

// ── Maven Central (Sonatype Central Portal) ──────────────────────────────────
// Credentials come from env vars in CI (release.yml -> publish-kotlin):
//   ORG_GRADLE_PROJECT_mavenCentralUsername / ...Password  (Sonatype token)
//   ORG_GRADLE_PROJECT_signingInMemoryKey / ...KeyPassword (GPG key + passphrase)
//
// The published jar carries libsekejap for five desktop targets under
// resources/natives/<os>-<arch>/, staged by that job from the build-native-libs
// artifacts, so a consumer needs no system install.
mavenPublishing {
    // An empty javadoc jar: Maven Central requires *a* javadoc artifact, and the
    // javadoc tool has nothing to say about a package whose public surface is
    // Kotlin. The sources jar carries the documentation.
    configure(KotlinJvm(javadocJar = JavadocJar.Empty(), sourcesJar = SourcesJar.Sources()))
    publishToMavenCentral(automaticRelease = true)
    signAllPublications()
    coordinates("life.sekejap", "sekejap-ffm", version.toString())
    pom {
        name.set("sekejap-ffm")
        description.set(
            "sekejap for the JVM -- an embedded graph-first multi-model database, "
                + "bound with Panama/FFM over the C ABI. Usable from Java and Kotlin."
        )
        url.set("https://sekejap.life")
        licenses {
            license {
                name.set("Apache-2.0")
                url.set("https://www.apache.org/licenses/LICENSE-2.0")
            }
            license {
                name.set("MIT")
                url.set("https://opensource.org/licenses/MIT")
            }
        }
        developers {
            developer {
                id.set("sekejapdb")
                name.set("sekejap")
                url.set("https://github.com/sekejapdb")
            }
        }
        scm {
            url.set("https://github.com/sekejapdb/sekejap")
            connection.set("scm:git:https://github.com/sekejapdb/sekejap.git")
        }
    }
}
