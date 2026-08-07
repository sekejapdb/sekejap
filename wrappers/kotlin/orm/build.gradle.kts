import com.vanniktech.maven.publish.JavadocJar
import com.vanniktech.maven.publish.KotlinJvm
import com.vanniktech.maven.publish.SonatypeHost

plugins {
    kotlin("jvm")
    id("com.google.devtools.ksp")
    id("com.vanniktech.maven.publish")
}

// Reverse-DNS of the brand domain sekejap.life (matches the Kotlin package).
group = "life.sekejap"
version = "0.1.0"

repositories { mavenCentral() }

dependencies {
    // `api`: the KSP-generated code (compiled in the consumer) references these.
    api("org.jetbrains.kotlinx:kotlinx-serialization-json:1.7.3")
    api("org.jetbrains.kotlinx:kotlinx-coroutines-core:1.9.0")

    testImplementation(kotlin("test"))
    testImplementation("org.jetbrains.kotlinx:kotlinx-coroutines-test:1.9.0")
    kspTest(project(":processor"))
}

kotlin { jvmToolchain(17) }

// Host tests load the JNI lib built for this machine.
val hostLib = file("../rust/target/release").absolutePath
tasks.test {
    useJUnitPlatform()
    val ext = if (System.getProperty("os.name").startsWith("Mac")) "dylib" else "so"
    systemProperty("sekejap.jni.path", "$hostLib/libsekejap_jni.$ext")
}

// ── Maven Central publishing (life.sekejap namespace — verify sekejap.life) ──
// Reuses the same Sonatype account + GPG key as the FFM binding; credentials come
// from CI env vars (ORG_GRADLE_PROJECT_mavenCentralUsername/Password, signingInMemoryKey…).
mavenPublishing {
    configure(KotlinJvm(javadocJar = JavadocJar.Empty(), sourcesJar = true))
    publishToMavenCentral(SonatypeHost.CENTRAL_PORTAL, automaticRelease = true)
    if (project.findProperty("signingInMemoryKey") != null) signAllPublications()
    coordinates("life.sekejap", "sekejap-orm", version.toString())
    pom {
        name.set("sekejap-orm")
        description.set("Typed, reactive Kotlin API for sekejap — @SekejapEntity + KSP + Flow, over the JNI core.")
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
