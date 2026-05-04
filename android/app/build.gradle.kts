plugins {
    id("com.android.application")
}

android {
    namespace = "net.iapetusservers.caviewer"
    compileSdk = 35
    ndkVersion = "30.0.14904198"

    defaultConfig {
        applicationId = "net.iapetusservers.caviewer"
        minSdk = 26
        targetSdk = 35
        versionCode = 1
        versionName = "0.1.0"

        ndk {
            abiFilters += "arm64-v8a"
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            // Sign release builds with the debug keystore so we can adb-install
            // them locally. NOT for distribution — Play Store requires a real
            // upload key.
            signingConfig = signingConfigs.getByName("debug")
        }
    }

    sourceSets {
        getByName("main") {
            jniLibs.srcDirs("src/main/jniLibs")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
}
