plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

val localReleaseKeystore = file("local-release.keystore")
val keytool = file(
    "${System.getProperty("java.home")}/bin/keytool" +
        if (System.getProperty("os.name").startsWith("Windows")) ".exe" else "",
)
val generateLocalReleaseKeystore by tasks.registering(Exec::class) {
    description = "Generates a reusable local signing key for release APKs."
    onlyIf { !localReleaseKeystore.exists() }
    outputs.file(localReleaseKeystore)
    commandLine(
        keytool,
        "-genkeypair",
        "-keystore", localReleaseKeystore,
        "-storetype", "PKCS12",
        "-storepass", "soundwave",
        "-alias", "soundwave",
        "-keypass", "soundwave",
        "-keyalg", "RSA",
        "-keysize", "2048",
        "-validity", "10000",
        "-dname", "CN=Soundwave Local Release, OU=Development, O=Soundwave, C=US",
        "-noprompt",
    )
}

dependencies {
    implementation("com.google.android.gms:play-services-code-scanner:16.1.0")
}

android {
    namespace = "com.example.audiostream"
    compileSdk = 36
    buildToolsVersion = "36.0.0"

    defaultConfig {
        applicationId = "com.example.audiostream"
        minSdk = 26
        targetSdk = 36
        versionCode = 1
        versionName = "0.1.0"

        ndk {
            abiFilters += listOf("arm64-v8a", "x86_64")
        }
    }

    signingConfigs {
        create("release") {
            storeFile = localReleaseKeystore
            storePassword = "soundwave"
            keyAlias = "soundwave"
            keyPassword = "soundwave"
        }
    }

    buildTypes {
        release {
            signingConfig = signingConfigs.getByName("release")
            isMinifyEnabled = false
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro",
            )
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions {
        jvmTarget = "17"
    }
}

val buildRustNative by tasks.registering(Exec::class) {
    group = "build"
    description = "Builds the Rust JNI library for Android with cargo-ndk."
    workingDir(file("src/main/rust"))
    commandLine(
        "cargo",
        "ndk",
        "-t",
        "arm64-v8a",
        "-t",
        "x86_64",
        "-o",
        "../jniLibs",
        "build",
        "--release",
    )
}

tasks.named("preBuild").configure {
    dependsOn(buildRustNative, generateLocalReleaseKeystore)
}
