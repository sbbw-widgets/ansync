plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.android)
    alias(libs.plugins.rust.android)
}

android {
    namespace = "org.gameros.ansync"
    compileSdk = libs.versions.compileSdk.get().toInt()
    buildToolsVersion = libs.versions.buildTools.get()
    ndkVersion = libs.versions.ndk.get()

    defaultConfig {
        applicationId = "org.gameros.ansync"
        // Android 8.0+ — required by AccessibilityService.dispatchGesture
        // and MediaProjection's persistent foreground service contract.
        minSdk = libs.versions.minSdk.get().toInt()
        targetSdk = libs.versions.targetSdk.get().toInt()
        versionCode = 1
        versionName = "0.1.0"
        ndk {
            // Single ABI for Step 7d initial bring-up; CI expands to
            // armeabi-v7a + x86_64 once the release pipeline lands.
            abiFilters += setOf("arm64-v8a")
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro"
            )
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.toVersion(libs.versions.java.get())
        targetCompatibility = JavaVersion.toVersion(libs.versions.java.get())
    }

    kotlinOptions {
        jvmTarget = libs.versions.java.get()
    }

    buildFeatures {
        compose = true
    }

    composeOptions {
        kotlinCompilerExtensionVersion = libs.versions.composeCompiler.get()
    }

    packaging {
        jniLibs {
            pickFirsts += setOf("**/libansync_companion_native.so")
        }
    }
}

// rust-android-gradle plugin: compiles `../Cargo.toml` to `.so` and
// drops it under `app/build/rustJniLibs/<abi>/` which AGP picks up
// via the standard JNI lib merge.
cargo {
    module = "../"
    libname = "ansync_companion_native"
    targets = listOf("arm64")
    targetDirectory = "../target"
    profile = if (gradle.startParameter.taskNames.any { it.contains("release", ignoreCase = true) }) {
        "release"
    } else {
        "debug"
    }
}

tasks.whenTaskAdded {
    if (name == "mergeDebugJniLibFolders" || name == "mergeReleaseJniLibFolders") {
        dependsOn("cargoBuild")
    }
}

dependencies {
    implementation(libs.androidx.core.ktx)
    implementation(libs.androidx.lifecycle.runtime.ktx)
    implementation(libs.androidx.activity.compose)
    implementation(platform(libs.androidx.compose.bom))
    implementation(libs.androidx.compose.ui)
    implementation(libs.androidx.compose.ui.graphics)
    implementation(libs.androidx.compose.ui.tooling.preview)
    implementation(libs.androidx.compose.material3)
}
