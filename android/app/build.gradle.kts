plugins {
    id("com.android.application")
}

val commitCount = providers.exec {
    commandLine("git", "rev-list", "--count", "HEAD")
    isIgnoreExitValue = true
}.standardOutput.asText.map { it.trim().toIntOrNull() ?: 1 }

android {
    namespace = "com.rutger.wsconnect"
    compileSdk = 37

    defaultConfig {
        applicationId = "com.rutger.wsconnect"
        minSdk = 33
        targetSdk = 36
        versionCode = commitCount.get()
        versionName = "v${commitCount.get()}"
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
