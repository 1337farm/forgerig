# ForgeRig

ForgeRig (formerly OneStopShop / OpenCode) is a standalone ARM64 Linux engine providing an isolated Linux userland on unrooted Android devices.

## Features

- **Isolated Linux Userland:** Boots a real aarch64 Alpine rootfs inside a PRoot environment; both the rootfs and the proot binary are extracted from the APK at install time.
- **Persistent Daemon Orchestration:** The rootfs launches a Rust orchestrator daemon (`forgerig-daemon`) that serves HTTP + WebSocket JSON-RPC on a dynamically allocated local port, kept awake by a background service.
- **Embedded Web UI:** The WebView connects to `http://127.0.0.1:$PORT`, gets a landing page from the daemon, and opens a WebSocket for prompts.
- **Seamless GitHub Integration:** Uses custom URI scheme routing (`opencode://oauth-callback`) to silently exchange auth codes and inject access tokens into the container's `.gitconfig`.

## Development

The Android host is built with Kotlin, and the daemon is built with Rust (`daemon/`).

### Building the container assets

The APK embeds a real proot binary, its loader, and a minirootfs with the compiled
daemon inside. These are generated (not committed):

```bash
# host toolchain (fast, for local checks)
bash scripts/prepare-assets.sh

# cross-build the daemon for the arm64 musl rootfs (as CI does)
rustup target add aarch64-unknown-linux-musl
FORGERIG_CARGO_TARGET=aarch64-unknown-linux-musl bash scripts/prepare-assets.sh
```

`prepare-assets.sh` fetches the pinned proot package and Alpine minirootfs from
their upstream mirrors, then writes `app/src/main/assets/{proot,proot-loader,ubuntu-rootfs.tar.gz}`.
Any APK packaging task fails loudly if these are missing or empty, so a stub or
half-fetched asset can never be shipped.

### Build and Test

```bash
bash scripts/prepare-assets.sh   # required before assembleDebug
./gradlew assembleDebug
./gradlew test
```

### CI / CD

A GitHub Actions workflow is provided to build and test the application on pushes to the `main` branch. Artifacts (APK) are uploaded to a `latest` release tag.
