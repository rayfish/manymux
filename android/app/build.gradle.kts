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
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
        // The app is still its first version, and stays on it until there is
        // something to ship. A number that moves per commit says nothing about
        // what is in the build and drifts away from the crate's own for no
        // reason anybody can read back.
        versionCode = 1
        versionName = "0.1.0"
        ndk {
            // arm64 for a phone, x86_64 for an emulator. Nothing 32 bit: the
            // shim is a fresh build with no old devices to answer to.
            abiFilters += providers.gradleProperty("targetAbis")
                .orElse("arm64-v8a,x86_64").get().split(",")
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = true
            isShrinkResources = true
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
        }
    }

    packaging {
        jniLibs.useLegacyPackaging = true
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
    implementation("net.java.dev.jna:jna:5.18.1@aar")
    implementation("androidx.annotation:annotation:1.9.1")
    androidTestImplementation("androidx.test:runner:1.6.2")
    androidTestImplementation("androidx.test.ext:junit:1.2.1")
    androidTestImplementation("androidx.test:core:1.6.1")
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
    inputs.file("${rootDir}/rust/uniffi.toml")
    inputs.file("${rootDir}/build-rust.sh")
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
