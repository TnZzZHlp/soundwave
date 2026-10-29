plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

val localReleaseKeystore = file("local-release.keystore")
val keytool = file(
    "${System.getProperty("java.home")}/bin/keytool" +
        if (System.getProperty("os.name").startsWith("Windows")) ".exe" else "",
)

// Release signing reads an externally supplied keystore when one is provided
// (CI secrets), otherwise it uses the reusable local keystore below.
fun signingSetting(environment: String, property: String): String? =
    System.getenv(environment)?.takeIf { it.isNotBlank() }
        ?: providers.gradleProperty(property).orNull?.takeIf { it.isNotBlank() }

val externalKeystorePath = signingSetting("SOUNDWAVE_KEYSTORE_PATH", "soundwaveKeystorePath")
val externalKeystorePassword = signingSetting("SOUNDWAVE_KEYSTORE_PASSWORD", "soundwaveKeystorePassword")
val externalKeyAlias = signingSetting("SOUNDWAVE_KEY_ALIAS", "soundwaveKeyAlias")
val externalKeyPassword = signingSetting("SOUNDWAVE_KEY_PASSWORD", "soundwaveKeyPassword")

val generateLocalReleaseKeystore by tasks.registering(Exec::class) {
    description = "Generates a reusable local signing key for release APKs."
    onlyIf { externalKeystorePath == null && !localReleaseKeystore.exists() }
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
    implementation("androidx.activity:activity:1.9.3")
    implementation("androidx.camera:camera-camera2:1.4.2")
    implementation("androidx.camera:camera-lifecycle:1.4.2")
    implementation("androidx.camera:camera-view:1.4.2")
    implementation("com.google.zxing:core:3.5.3")
}

android {
    namespace = "com.example.audiostream"
    compileSdk = 36
    buildToolsVersion = "36.0.0"

    defaultConfig {
        applicationId = "com.example.audiostream"
        minSdk = 26
        targetSdk = 36
        versionCode = providers.gradleProperty("versionCode").orNull?.toIntOrNull() ?: 1
        versionName = providers.gradleProperty("versionName").orNull ?: "0.2.1"

        ndk {
            abiFilters += listOf("arm64-v8a", "x86_64")
        }
    }

    signingConfigs {
        create("release") {
            if (externalKeystorePath != null) {
                storeFile = file(externalKeystorePath)
                storePassword = externalKeystorePassword
                keyAlias = externalKeyAlias
                keyPassword = externalKeyPassword
            } else {
                storeFile = localReleaseKeystore
                storePassword = "soundwave"
                keyAlias = "soundwave"
                keyPassword = "soundwave"
            }
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
