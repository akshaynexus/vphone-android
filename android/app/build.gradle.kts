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
        versionCode = 1
        versionName = "0.1"
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

    packaging {
        jniLibs {
            // Real files on disk: libadrenotools and the driver's namespaces load them by path.
            useLegacyPackaging = true
            keepDebugSymbols += "**/*.so"
        }
    }
}
