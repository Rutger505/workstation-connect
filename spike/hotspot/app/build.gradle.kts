plugins {
    id("com.android.application")
}

android {
    namespace = "com.rutger.wsconnect.spike"
    compileSdk = 37

    defaultConfig {
        applicationId = "com.rutger.wsconnect.spike"
        minSdk = 33
        targetSdk = 36
        versionCode = 1
        versionName = "phase0-1"
    }

    buildFeatures {
        aidl = true
        buildConfig = true
    }
}

dependencies {
    implementation("dev.rikka.shizuku:api:13.1.5")
    implementation("dev.rikka.shizuku:provider:13.1.5")
    implementation("org.lsposed.hiddenapibypass:hiddenapibypass:6.2")
}
