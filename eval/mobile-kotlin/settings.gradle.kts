pluginManagement {
    repositories {
        google()
        mavenCentral()
        gradlePluginPortal()
    }
}
dependencyResolutionManagement {
    repositoriesMode.set(RepositoriesMode.PREFER_SETTINGS)
    repositories {
        google()
        mavenCentral()
    }
}
rootProject.name = "sekejap-kotlin-bench"
include(":app")

// The ergonomic typed layer + its KSP processor, as a composite build.
includeBuild("../../wrappers/kotlin/orm")
