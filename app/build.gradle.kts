import java.util.Properties
import java.io.File
import java.io.FileInputStream
import java.io.FileNotFoundException

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

val localProperties = Properties()
try {
    localProperties.load(FileInputStream(rootProject.file("local.properties")))
} catch (e: FileNotFoundException) {
    // Ignore if not present
}

// Short commit hash baked into the APK filename (farm-style:
// forgerig-<buildtype>-<sha>.apk) and BuildConfig.GIT_SHA.
val gitCommitHash: String = try {
    val proc = ProcessBuilder("git", "rev-parse", "--short=10", "HEAD")
        .directory(rootProject.projectDir)
        .redirectErrorStream(true)
        .start()
    val sha = proc.inputStream.bufferedReader().readText().trim()
    proc.waitFor()
    if (sha.matches(Regex("[0-9a-f]{10}"))) sha else "dev"
} catch (e: Exception) {
    "dev"
}

android {
    namespace = "com.onestopshop"
    compileSdk = 34

    defaultConfig {
        applicationId = "com.onestopshop"
        minSdk = 26
        targetSdk = 34
        versionCode = 1
        versionName = "1.0"

        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"

        buildConfigField("String", "GITHUB_CLIENT_ID", "\"${localProperties.getProperty("GITHUB_CLIENT_ID", "")}\"")
        buildConfigField("String", "GITHUB_CLIENT_SECRET", "\"${localProperties.getProperty("GITHUB_CLIENT_SECRET", "")}\"")
        buildConfigField("String", "GIT_SHA", "\"$gitCommitHash\"")
    }

    androidResources {
        // Keep the gzipped rootfs blob stored as-is instead of recompressing it.
        noCompress += "bin"
    }

    buildFeatures {
        buildConfig = true
    }

    signingConfigs {
        val ks = Properties()
        val ksf = rootProject.file("keystore.properties")
        if (ksf.exists()) {
            ksf.inputStream().use { ks.load(it) }
            create("farm") {
                storeFile = file(ks.getProperty("storeFile"))
                storePassword = ks.getProperty("storePassword")
                keyAlias = ks.getProperty("keyAlias")
                keyPassword = ks.getProperty("keyPassword")
            }
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro"
            )
            signingConfig = signingConfigs.getByName("farm")
        }
        debug {
            signingConfig = signingConfigs.getByName("farm")
        }
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_1_8
        targetCompatibility = JavaVersion.VERSION_1_8
    }
    kotlinOptions {
        jvmTarget = "1.8"
    }
}

dependencies {
    implementation("androidx.core:core-ktx:1.12.0")
    implementation("androidx.appcompat:appcompat:1.6.1")
    implementation("com.google.android.material:material:1.11.0")
    implementation("androidx.constraintlayout:constraintlayout:2.1.4")
    implementation("org.apache.commons:commons-compress:1.24.0")
    testImplementation("junit:junit:4.13.2")
    androidTestImplementation("androidx.test.ext:junit:1.1.5")
    androidTestImplementation("androidx.test.espresso:espresso-core:3.5.1")
}

// Reject APKs built with placeholder container assets (see scripts/prepare-assets.sh).
// proot ships as native libs (PackageManager extracts them executable);
// only the rootfs blob lives in assets.
val checkContainerAssets by tasks.registering(Exec::class) {
    workingDir = rootProject.projectDir
    commandLine("sh", "-c",
        "test -s app/src/main/jniLibs/arm64-v8a/libproot.so && " +
        "test -s app/src/main/jniLibs/arm64-v8a/libproot_loader.so && " +
        "test -s app/src/main/jniLibs/arm64-v8a/libandroid-shmem.so && " +
        "test -s app/src/main/assets/libtalloc.so.2 && " +
        "(test -s app/src/main/assets/ubuntu-rootfs.bin || test -s app/src/main/assets/ubuntu-rootfs.tar.gz)")
}
// Only packaging needs the assets; unit tests must stay runnable without them.
// `testDebugUnitTest` happens to pull the whole assemble<bool> graph (including
// packageFoo), so the test job opts out explicitly with -PskipContainerAssetsCheck.
tasks.matching { it.name == "packageDebug" || it.name == "packageRelease" }
    .configureEach { dependsOn(checkContainerAssets) }
if (providers.gradleProperty("skipContainerAssetsCheck").isPresent) {
    tasks.named("checkContainerAssets").configure { enabled = false }
}

// Rename assembled APKs to forgerig-<buildtype>-<sha>.apk (farm-style), so
// every build artifact carries its commit hash. Plain task + finalizedBy
// (no applicationVariants DSL) to stay compatible with newer AGP versions.
val renameApkWithHash by tasks.registering {
    doLast {
        val outDirs = listOf("debug", "release").map { type ->
            layout.buildDirectory.dir("outputs/apk/$type").get().asFile
        }
        outDirs.forEach { outDir ->
            outDir.listFiles { f -> f.isFile && f.name.startsWith("app-") && f.name.endsWith(".apk") }
                ?.forEach { apk ->
                    val type = outDir.name
                    val target = File(outDir, "forgerig-$type-$gitCommitHash.apk")
                    if (apk.canonicalPath != target.canonicalPath) {
                        if (!apk.renameTo(target)) {
                            throw GradleException("Failed to rename ${apk.name} to ${target.name}")
                        }
                        logger.lifecycle("Renamed ${apk.name} -> ${target.name}")
                    }
                }
        }
        val hashed = outDirs.flatMap { dir ->
            dir.listFiles { f -> f.isFile && f.name.startsWith("forgerig-") && f.name.endsWith(".apk") }
                ?.toList() ?: emptyList()
        }
        if (hashed.isEmpty()) {
            // Warn only: finalizedBy also runs when assemble itself failed,
            // and must not mask the real error. CI globs forgerig-*.apk and
            // fails loudly if the rename never happened on a green build.
            logger.warn("No forgerig-*.apk found after assemble task")
        }
    }
}
tasks.matching { it.name == "assembleDebug" || it.name == "assembleRelease" || it.name == "assemble" }
    .configureEach { finalizedBy(renameApkWithHash) }
