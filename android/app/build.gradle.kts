plugins {
    id("com.android.application")
}

android {
    namespace = "com.vphone.android"
    compileSdk = 37

    defaultConfig {
        applicationId = "com.vphone.android"
        minSdk = 35
        targetSdk = 37
        versionCode = 13
        versionName = "0.4.2"
        ndk {
            abiFilters += "arm64-v8a"
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    sourceSets {
        getByName("main") {
            // Staged by scripts/build-apk.sh.
            jniLibs.directories.add(rootDir.resolve("build/jniLibs").path)
        }
    }

    buildFeatures {
        viewBinding = false
        buildConfig = true
    }

    packaging {
        jniLibs {
            // Real files on disk: libadrenotools and the driver's namespaces load them by path.
            useLegacyPackaging = true
            keepDebugSymbols += "**/*.so"
        }
    }
}

dependencies {
    // The wizard only: the VM screen stays framework-only.
    implementation("androidx.appcompat:appcompat:1.7.1")
    implementation("androidx.activity:activity-ktx:1.10.1")
    implementation("com.google.android.material:material:1.12.0")
}
