// sekejap Kotlin/JVM binding — Panama / FFM (Foreign Function & Memory, JDK 22+)
// over the C ABI (libsekejap). Pure JVM, no native glue, no JNA.
// Dev: build libsekejap first (`cargo build --release -p sekejap-capi`), then `gradle test`.
// Published jar bundles native libs under resources/natives/<os>-<arch>/ (see CI).

import com.vanniktech.maven.publish.JavadocJar
import com.vanniktech.maven.publish.KotlinJvm
import com.vanniktech.maven.publish.SonatypeHost

plugins {
    kotlin("jvm") version "2.0.20"
    application
    id("com.vanniktech.maven.publish") version "0.30.0"
}

group = "com.zebflow"           // reverse-DNS of the zebflow.com umbrella namespace
version = "0.13.2"

repositories { mavenCentral() }

dependencies {
    testImplementation(kotlin("test"))
}

// FFM needs JDK 22+ (finalized in 22). Kotlin doesn't support the very newest JDKs,
// so 22 is the sweet spot. Gradle auto-detects installed JDKs.
kotlin {
    jvmToolchain(22)
}

val libDir = file("../../target/release").absolutePath

// Both test and the bench 'run' need the native-access flag (suppresses FFM's
// restricted-method warnings) and the directory holding libsekejap.
val ffmArgs = listOf("--enable-native-access=ALL-UNNAMED", "-Djava.library.path=$libDir")

tasks.test {
    useJUnitPlatform()
    jvmArgs(ffmArgs)
}

// Micro-benchmark: `gradle run -q` (see Bench.kt) — exercises the real FFM binding.
application {
    mainClass.set("BenchKt")
    applicationDefaultJvmArgs = ffmArgs
}
tasks.named<JavaExec>("run") {
    jvmArgs(ffmArgs)
}

// ── Maven Central publishing (Sonatype Central Portal) ─────────────────────
// Credentials come from env vars in CI (release.yml → publish-kotlin):
//   ORG_GRADLE_PROJECT_mavenCentralUsername / ...Password  (Sonatype token)
//   ORG_GRADLE_PROJECT_signingInMemoryKey / ...KeyPassword (GPG key + passphrase)
mavenPublishing {
    // Empty javadoc jar — Maven Central only requires *a* javadoc artifact, and the
    // real `javadoc` tool chokes on the java.lang.foreign (FFM) code in Ffi.java.
    configure(KotlinJvm(javadocJar = JavadocJar.Empty(), sourcesJar = true))
    publishToMavenCentral(SonatypeHost.CENTRAL_PORTAL, automaticRelease = true)
    signAllPublications()
    coordinates(group.toString(), "sekejap", version.toString())
    pom {
        name.set("sekejap")
        description.set("Embedded graph-first multi-model database — Kotlin/JVM binding (Panama/FFM).")
        url.set("https://github.com/sekejapdb/sekejap")
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
