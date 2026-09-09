#!/usr/bin/env bash
# Prepare the Android container assets that the app extracts at install time.
#
# Produces, under $FORGERIG_ASSETS_DIR (default: app/src/main/assets):
#   proot                aarch64 proot (from the Termux package repo)
#   proot-loader         the proot loader companion, next to the binary
#   ubuntu-rootfs.bin    a real aarch64 Linux rootfs (Alpine minirootfs, gzipped
#                        tar) with the ForgeRig daemon baked in at
#                        usr/bin/forgerig-daemon and an entrypoint at
#                        root/start.sh
# NOTE: the rootfs uses a `.bin` extension (not `.tar.gz`) because the
# Android Gradle Plugin automatically gunzips `.gz` assets during the merge
# step, which renames `ubuntu-rootfs.tar.gz` to `ubuntu-rootfs.tar` inside
# the APK and breaks AssetManager.open("ubuntu-rootfs.tar.gz").
#
# The daemon is built with `cargo build --release`. By default it uses the
# host toolchain. Cross-build with:
#   export FORGERIG_CARGO_TARGET=aarch64-unknown-linux-musl
# (as the GitHub runner does, so the binary runs inside the musl rootfs).
# Pass extra cargo features with FORGERIG_CARGO_FEATURES (space-separated);
# the CI musl build sets it to "vendored-openssl".
#
# Overrides for offline/pinned use:
#   FORGERIG_PROOT_DEB_URL   direct URL to a proot_*_aarch64.deb
#   FORGERIG_ALPINE_URL      direct URL to an alpine-minirootfs-*-aarch64.tar.gz
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ASSETS="${FORGERIG_ASSETS_DIR:-$ROOT/app/src/main/assets}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

echo ">> Preparing container assets"

# --- 1. Build the daemon -----------------------------------------------------
DAEMON_TARGET="${FORGERIG_CARGO_TARGET:-}"
CARGO_FEATURES="${FORGERIG_CARGO_FEATURES:-}"
CARGO_ARGS=""
if [ -n "$CARGO_FEATURES" ]; then
  CARGO_ARGS="--features $CARGO_FEATURES"
fi
if [ -n "$DAEMON_TARGET" ]; then
  echo ">> Building daemon for $DAEMON_TARGET"
  ( cd "$ROOT/daemon" && cargo build --release --target "$DAEMON_TARGET" $CARGO_ARGS )
  DAEMON_BIN="$ROOT/daemon/target/$DAEMON_TARGET/release/daemon"
else
  echo ">> Building daemon for the host target"
  ( cd "$ROOT/daemon" && cargo build --release $CARGO_ARGS )
  DAEMON_BIN="$ROOT/daemon/target/release/daemon"
fi
[ -x "$DAEMON_BIN" ] || { echo "ERROR: daemon binary missing: $DAEMON_BIN" >&2; exit 1; }

# --- 2. Fetch proot (Termux aarch64 package) ---------------------------------
mkdir -p "$WORK/proot/deb"
if [ -n "${FORGERIG_PROOT_DEB_URL:-}" ]; then
  DEB_URL="$FORGERIG_PROOT_DEB_URL"
else
  POOL="https://packages.termux.dev/apt/termux-main/pool/main/p/proot/"
  DEB="$(curl -fsSL "$POOL" | grep -o 'proot_[0-9][^"]*_aarch64\.deb' | sort -V | tail -1)"
  [ -n "$DEB" ] || { echo "ERROR: could not find a proot aarch64 .deb in $POOL" >&2; exit 1; }
  DEB_URL="$POOL$DEB"
fi
echo ">> Fetching proot from $DEB_URL"
curl -fsSL -o "$WORK/proot.deb" "$DEB_URL"
( cd "$WORK/proot/deb" && ar x "$WORK/proot.deb" data.tar.xz && tar xf data.tar.xz )
PROOT_BIN="$(find "$WORK/proot/deb" -path '*usr/bin/proot' | head -1)"
PROOT_LOADER="$(find "$WORK/proot/deb" -path '*libexec/proot/loader' | head -1)"
[ -n "$PROOT_BIN" ] && [ -n "$PROOT_LOADER" ] \
  || { echo "ERROR: proot binary or loader missing from .deb" >&2; exit 1; }

# --- 3. Fetch a real aarch64 rootfs (Alpine minirootfs) ----------------------
if [ -n "${FORGERIG_ALPINE_URL:-}" ]; then
  ROOTFS_URL="$FORGERIG_ALPINE_URL"
else
  REL_DIR="https://dl-cdn.alpinelinux.org/alpine/latest-stable/releases/aarch64/"
  ROOTFS_ARCHIVE="$(curl -fsSL "$REL_DIR" | grep -o 'alpine-minirootfs-[0-9.]*-aarch64\.tar\.gz' | sort -V | tail -1)"
  [ -n "$ROOTFS_ARCHIVE" ] || { echo "ERROR: could not find an Alpine minirootfs" >&2; exit 1; }
  ROOTFS_URL="$REL_DIR$ROOTFS_ARCHIVE"
fi
echo ">> Fetching rootfs from $ROOTFS_URL"
ROOTFS_STAGING="$WORK/rootfs"
mkdir -p "$ROOTFS_STAGING"
curl -fsSL -o "$WORK/rootfs.tar.gz" "$ROOTFS_URL"
tar xzf "$WORK/rootfs.tar.gz" -C "$ROOTFS_STAGING"

# --- 4. Bake the daemon + entrypoint into the rootfs -------------------------
mkdir -p "$ROOTFS_STAGING/usr/bin" "$ROOTFS_STAGING/root"
cp "$DAEMON_BIN" "$ROOTFS_STAGING/usr/bin/forgerig-daemon"
chmod 755 "$ROOTFS_STAGING/usr/bin/forgerig-daemon"
cat > "$ROOTFS_STAGING/root/start.sh" <<'SH'
#!/bin/sh
# ForgeRig container entrypoint. Launches the orchestrator daemon, which
# serves HTTP + WebSocket JSON-RPC on the port given in $PORT.
set -e
if [ ! -x /usr/bin/forgerig-daemon ]; then
  echo "forgerig-daemon missing" >&2
  exit 1
fi
exec /usr/bin/forgerig-daemon
SH
chmod 755 "$ROOTFS_STAGING/root/start.sh"

# --- 5. Emit the assets ------------------------------------------------------
mkdir -p "$ASSETS"
echo ">> Writing $ASSETS/ubuntu-rootfs.bin (gzipped tar, .bin extension avoids AGP gunzipping)"
tar czf "$ASSETS/ubuntu-rootfs.bin" -C "$ROOTFS_STAGING" .
rm -f "$ASSETS/ubuntu-rootfs.tar.gz"
echo ">> Writing $ASSETS/proot and $ASSETS/proot-loader"
cp "$PROOT_BIN" "$ASSETS/proot"
cp "$PROOT_LOADER" "$ASSETS/proot-loader"
chmod 755 "$ASSETS/proot" "$ASSETS/proot-loader"

echo ">> Done. Assets:"
ls -l "$ASSETS/proot" "$ASSETS/proot-loader" "$ASSETS/ubuntu-rootfs.bin"