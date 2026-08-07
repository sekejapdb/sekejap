import com.vanniktech.maven.publish.JavadocJar
import com.vanniktech.maven.publish.KotlinJvm
import com.vanniktech.maven.publish.SonatypeHost

plugins {
    kotlin("jvm")
    id("com.vanniktech.maven.publish")
}

group = "life.sekejap"
// Single source of truth: the workspace version in the root Cargo.toml.
version = rootDir.resolve("../../../Cargo.toml").readLines()
    .first { it.trimStart().startsWith("version = ") }
    .substringAfter('"').substringBefore('"')

repositories { mavenCentral() }

dependencies {
    implementation("com.google.devtools.ksp:symbol-processing-api:2.0.21-1.0.28")
}

kotlin { jvmToolchain(17) }

mavenPublishing {
    configure(KotlinJvm(javadocJar = JavadocJar.Empty(), sourcesJar = true))
    publishToMavenCentral(SonatypeHost.CENTRAL_PORTAL, automaticRelease = true)
    if (project.findProperty("signingInMemoryKey") != null) signAllPublications()
    coordinates("life.sekejap", "sekejap-processor", version.toString())
    pom {
        name.set("sekejap-processor")
        description.set("KSP processor generating the typed sekejap collection API from @SekejapEntity.")
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
