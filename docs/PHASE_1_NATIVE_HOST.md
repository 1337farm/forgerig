# Phase 1: Native Android Host & Container Runtime

## Native Package Assets & Automated Unpacking
 * Bundle the pre-compiled proot ARM64 binary and the minimal ubuntu-rootfs.tar.gz archive directly within the Android application package assets (assets/ or res/raw/).
 * Implement the native Kotlin AssetExtractor utilizing GZIPInputStream wrapped with Apache Commons TarArchiveInputStream.
 * Extract the root filesystem directly into the app's internal sandboxed directory (context.filesDir.absolutePath + "/ubuntu_rootfs") on the initial "Install Now" tap.
 * Set executable permissions (outputFile.setExecutable(true, false)) on container binaries during stream inflation.

## Container Lifecycle & Connection Persistence
 * Wrap the PRoot container process inside an Android Foreground Service equipped with PARTIAL_WAKE_LOCK and explicit Wi-Fi locks to eliminate OS background thread suspension.
 * Configure the embedded Android WebView client to prevent background sleep cycles, ensuring persistent WebSocket communication between the UI and local daemons.
 * Implement native GitHub OAuth orchestration using custom URI callbacks (forgerig://oauth-callback), automatically exchanging the auth code for a token and injecting it into /root/.gitconfig.