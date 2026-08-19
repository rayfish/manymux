plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "dev.manymux.phone"
    compileSdk = 36

    defaultConfig {
        applicationId = "dev.manymux.phone"
        // The oldest Android the root crate's own target supports.
        minSdk = 24
        targetSdk = 36
        versionCode = 4
        versionName = "0.2.2"
        ndk {
            // arm64 for a phone, x86_64 for an emulator. Nothing 32 bit: the
            // shim is a fresh build with no old devices to answer to.
            abiFilters += listOf("arm64-v8a", "x86_64")
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    kotlinOptions {
        jvmTarget = "17"
    }

    sourceSets {
        getByName("main") {
            // Written by `cargoBuild` below rather than checked in: what the
            // app calls has to be generated from the library it links, or the
            // two drift and the first anybody hears of it is a crash.
            kotlin.srcDir(layout.buildDirectory.dir("rust/uniffi"))
            jniLibs.srcDir(layout.buildDirectory.dir("rust/jniLibs"))
        }
    }
}

dependencies {
    // uniffi's Kotlin calls the library through JNA.
    implementation("net.java.dev.jna:jna:5.14.0@aar")
}

/// Build the Rust library for each ABI and generate the Kotlin that calls it.
val cargoBuild by tasks.registering(Exec::class) {
    workingDir = rootDir
    commandLine(
        "bash",
        "build-rust.sh",
        layout.buildDirectory.dir("rust").get().asFile.absolutePath,
    )
    inputs.dir("${rootDir}/rust/src")
    inputs.file("${rootDir}/rust/Cargo.toml")
    inputs.file("${rootDir}/rust/Cargo.lock")
    // The crate one directory further up is most of what gets linked, and the
    // whole claim of this arrangement is that a fix there is a fix here. Left
    // undeclared, a change to the client core leaves Gradle calling this task
    // up to date and packaging the library from before it.
    inputs.dir("${rootDir}/../src")
    inputs.file("${rootDir}/../Cargo.toml")
    inputs.file("${rootDir}/../Cargo.lock")
    outputs.dir(layout.buildDirectory.dir("rust"))
}

tasks.withType<com.android.build.gradle.tasks.MergeSourceSetFolders>().configureEach {
    dependsOn(cargoBuild)
}

tasks.withType<org.jetbrains.kotlin.gradle.tasks.KotlinCompile>().configureEach {
    dependsOn(cargoBuild)
}
