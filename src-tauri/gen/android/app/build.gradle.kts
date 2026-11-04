import java.util.Properties

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("rust")
}

val tauriProperties = Properties().apply {
    val propFile = file("tauri.properties")
    if (propFile.exists()) {
        propFile.inputStream().use { load(it) }
    }
}

// Release (Play Store) signing. Secrets live in gen/android/key.properties
// (gitignored — NEVER commit it or the .jks). Provide:
//   storeFile=/abs/path/to/corvux-upload.jks
//   storePassword=...
//   keyAlias=corvux-upload
//   keyPassword=...
// When the file is absent we fall back to the debug keystore below, so
// on-device `pnpm tauri android build --apk` still works with zero setup.
val keystoreProperties = Properties().apply {
    val propFile = rootProject.file("key.properties")
    if (propFile.exists()) {
        propFile.inputStream().use { load(it) }
    }
}

android {
    // compileSdk 36 / targetSdk 36 = Android 16 — now a STABLE release and REQUIRED
    // by Google Play for new submissions from 2026-08-31 (min is 35 today). Bumped
    // from 34 on 2026-05-30.
    // Historical note: targetSdk was held at 34 because API 36 used to be a PREVIEW
    // SDK, and targeting a preview level caused "incompatible" install errors on
    // stable Android 14 devices. That no longer applies now 36 is stable.
    // ⚠ After this bump, RE-TEST on a real device (esp. the 32-bit Android 14 M13):
    // API 35+ enforces edge-to-edge display, so verify the UI isn't clipped under
    // the status/nav bars, and that notifications + exact-alarm reminders still fire.
    compileSdk = 36
    namespace = "com.corvuxmindware.corvux"
    // Pin the NDK so 16 KB page-size alignment can't silently regress.
    // Google Play requires apps targeting SDK 35+ to support 16 KB pages
    // on 64-bit ABIs. NDK r28+ emits 16 KB-aligned LOAD segments by
    // default; r27 and earlier emit 4 KB and would be REJECTED. Without a
    // pin the build uses whatever NDK the machine has — the arm64 .so was
    // verified 16 KB-aligned (0x4000) on r30 (audit 2026-06-08), but a
    // different/older NDK would regress with no error. Pinning turns a
    // missing/older NDK into a loud build failure. Keep this >= r28 when
    // bumping.
    ndkVersion = "30.0.14904198"
    defaultConfig {
        manifestPlaceholders["usesCleartextTraffic"] = "false"
        applicationId = "com.corvuxmindware.corvux"
        minSdk = 24
        targetSdk = 36
        versionCode = tauriProperties.getProperty("tauri.android.versionCode", "1").toInt()
        versionName = tauriProperties.getProperty("tauri.android.versionName", "1.0")
    }
    // Sign release builds with Android's debug keystore by default so
    // `pnpm tauri android build --apk` produces an installable APK for
    // on-device testing without a Play-Store keystore setup. For actual
    // Play Store uploads, swap this for a real release keystore loaded
    // from ~/.gradle/gradle.properties (never commit the .jks file).
    signingConfigs {
        create("localRelease") {
            val home = System.getProperty("user.home")
            storeFile = file("$home/.android/debug.keystore")
            storePassword = "android"
            keyAlias = "androiddebugkey"
            keyPassword = "android"
        }
        // Real Play-upload signing — only created when key.properties exists.
        if (keystoreProperties.getProperty("storeFile") != null) {
            create("release") {
                storeFile = file(keystoreProperties.getProperty("storeFile"))
                storePassword = keystoreProperties.getProperty("storePassword")
                keyAlias = keystoreProperties.getProperty("keyAlias")
                keyPassword = keystoreProperties.getProperty("keyPassword")
            }
        }
    }

    buildTypes {
        getByName("debug") {
            manifestPlaceholders["usesCleartextTraffic"] = "true"
            isDebuggable = true
            isJniDebuggable = true
            isMinifyEnabled = false
            // Don't keep debug symbols in the packaged .so — the Rust
            // staticlib carries them until stripped. Keeping them bloats
            // each ABI's .so by ~100 MB (libcorvux_lib.so goes from ~30 MB
            // stripped to ~150 MB with symbols). Desktop-side `cargo`
            // profile already handles the actual build; on-device debug
            // symbols aren't useful here.
        }
        getByName("release") {
            isMinifyEnabled = true
            proguardFiles(
                *fileTree(".") { include("**/*.pro") }
                    .plus(getDefaultProguardFile("proguard-android-optimize.txt"))
                    .toList().toTypedArray()
            )
            // Prefer the real release key when configured; else debug keystore
            // (debug-signed AABs are fine for testing but Play will reject them).
            signingConfig = signingConfigs.findByName("release")
                ?: signingConfigs.getByName("localRelease")
        }
    }
    kotlinOptions {
        jvmTarget = "1.8"
    }
    buildFeatures {
        buildConfig = true
    }
}

rust {
    rootDirRel = "../../../"
}

dependencies {
    implementation("androidx.webkit:webkit:1.14.0")
    implementation("androidx.appcompat:appcompat:1.7.1")
    implementation("androidx.activity:activity-ktx:1.10.1")
    implementation("com.google.android.material:material:1.12.0")
    testImplementation("junit:junit:4.13.2")
    androidTestImplementation("androidx.test.ext:junit:1.1.4")
    androidTestImplementation("androidx.test.espresso:espresso-core:3.5.0")
}

apply(from = "tauri.build.gradle.kts")