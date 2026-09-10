#!/usr/bin/env bash
# Prepare the Android container assets that the app extracts at install time.
#
# Produces:
#   app/src/main/assets/ubuntu-rootfs.bin   Ubuntu (glibc) aarch64 rootfs, gzipped tar
#   app/src/main/assets/libtalloc.so.2      proot DT_NEEDED dep (asset; name has no .so)
#   app/src/main/jniLibs/arm64-v8a/         proot + loader + deps + the daemon
#     libproot.so, libproot_loader.so, libandroid-shmem.so, libforgerig_daemon.so
#
# NOTE: the rootfs uses a `.bin` extension (not `.tar.gz`) because the Android
# Gradle Plugin auto-gunzips `.gz` assets at merge time, renaming the entry to
# `ubuntu-rootfs.tar` and breaking AssetManager.open("ubuntu-rootfs.tar.gz").
#
# The daemon runs HOST-side (a static musl binary exec'd straight from the
# app's native lib dir) and drives the work guest through proot, so it ships
# as a jniLib rather than being baked into the rootfs.
# Cross-build it with:
#   export FORGERIG_CARGO_TARGET=aarch64-unknown-linux-musl
# (the GitHub runner does this). Pass extra cargo features via
# FORGERIG_CARGO_FEATURES (CI sets "vendored-openssl").
#
# Overrides for offline/pinned use:
#   FORGERIG_PROOT_DEB_URL   direct URL to a proot_*_aarch64.deb
#   FORGERIG_UBUNTU_URL      direct URL to a prebuilt Ubuntu rootfs tarball
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ASSETS="${FORGERIG_ASSETS_DIR:-$ROOT/app/src/main/assets}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

echo ">> Preparing container assets"

# Run as root when we can (needed for debootstrap/qemu); no-op otherwise.
S() { if [ "$(id -u)" -eq 0 ]; then "$@"; else sudo "$@" 2>/dev/null || "$@"; fi; }

# --- 1. Build the daemon (host-side orchestrator, static musl) ----------------
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

# --- 2. Fetch proot + runtime deps (Termux aarch64 packages) ------------------
# proot is dynamically linked against libtalloc.so.2 and libandroid-shmem.so
# (DT_NEEDED), and its RUNPATH points at /data/data/com.termux/... which the
# app UID cannot read. We ship the two .so alongside proot and point
# LD_LIBRARY_PATH at the app's native lib dir at launch.
fetch_termux_deb() {
  local OUT="$1" POOL="$2" NAME_PATTERN="$3"
  local DEB
  DEB="$(curl -fsSL "$POOL" | grep -o "$NAME_PATTERN" | sort -V | tail -1)"
  [ -n "$DEB" ] || { echo "ERROR: could not find a matching .deb in $POOL" >&2; return 1; }
  echo ">> Fetching $POOL$DEB"
  curl -fsSL -o "$OUT" "$POOL$DEB"
}

mkdir -p "$WORK/proot/deb"
if [ -n "${FORGERIG_PROOT_DEB_URL:-}" ]; then
  echo ">> Fetching proot from $FORGERIG_PROOT_DEB_URL"
  curl -fsSL -o "$WORK/proot.deb" "$FORGERIG_PROOT_DEB_URL"
else
  fetch_termux_deb "$WORK/proot.deb" \
    "https://packages.termux.dev/apt/termux-main/pool/main/p/proot/" \
    'proot_[0-9][^"]*_aarch64\.deb'
fi
( cd "$WORK/proot/deb" && ar x "$WORK/proot.deb" data.tar.xz && tar xf data.tar.xz )
PROOT_BIN="$(find "$WORK/proot/deb" -path '*usr/bin/proot' | head -1)"
PROOT_LOADER="$(find "$WORK/proot/deb" -path '*libexec/proot/loader' | head -1)"
[ -n "$PROOT_BIN" ] && [ -n "$PROOT_LOADER" ] \
  || { echo "ERROR: proot binary or loader missing from .deb" >&2; exit 1; }

mkdir -p "$WORK/talloc/deb"
fetch_termux_deb "$WORK/talloc.deb" \
  "https://packages.termux.dev/apt/termux-main/pool/main/libt/libtalloc/" \
  'libtalloc_[0-9][^"]*_aarch64\.deb'
( cd "$WORK/talloc/deb" && ar x "$WORK/talloc.deb" data.tar.xz && tar xf data.tar.xz )
LIBTALLOC="$(find "$WORK/talloc/deb" -name 'libtalloc.so.2.*' | head -1)"
[ -n "$LIBTALLOC" ] \
  || { echo "ERROR: libtalloc.so.2.* missing from .deb" >&2; exit 1; }

mkdir -p "$WORK/shmem/deb"
fetch_termux_deb "$WORK/shmem.deb" \
  "https://packages.termux.dev/apt/termux-main/pool/main/liba/libandroid-shmem/" \
  'libandroid-shmem_[0-9][^"]*_aarch64\.deb'
( cd "$WORK/shmem/deb" && ar x "$WORK/shmem.deb" data.tar.xz && tar xf data.tar.xz )
LIBSHMEM="$(find "$WORK/shmem/deb" -name 'libandroid-shmem.so*' | head -1)"
[ -n "$LIBSHMEM" ] \
  || { echo "ERROR: libandroid-shmem.so missing from .deb" >&2; exit 1; }

# --- 3. Build an Ubuntu (glibc) aarch64 rootfs -------------------------------
ROOTFS_STAGING="$WORK/rootfs"
mkdir -p "$ROOTFS_STAGING"

if [ -n "${FORGERIG_UBUNTU_URL:-}" ]; then
  echo ">> Using pinned Ubuntu rootfs from $FORGERIG_UBUNTU_URL"
  curl -fsSL -o "$WORK/rootfs.tar.gz" "$FORGERIG_UBUNTU_URL"
  tar xzf "$WORK/rootfs.tar.gz" -C "$ROOTFS_STAGING"
elif command -v debootstrap >/dev/null 2>&1 || (command -v sudo >/dev/null 2>&1 && sudo -n true 2>/dev/null); then
  # On a Debian/Ubuntu host, debootstrap a foreign-arch rootfs with the tools
  # the work guest needs. qemu-user-static lets maintainer scripts run.
  UBU_CODENAME="${FORGERIG_UBUNTU_CODENAME:-noble}"
  UBU_MIRROR="${FORGERIG_UBUNTU_MIRROR:-http://ports.ubuntu.com/ubuntu-ports}"
  S apt-get update -y -qq || true
  S apt-get install -y -qq debootstrap qemu-user-static ca-certificates \
    || { echo "WARNING: could not install debootstrap/qemu-user-static; falling back to raw ubuntu-base" >&2; }
  if command -v debootstrap >/dev/null 2>&1; then
    S update-binfmts --enable qemu-aarch64 2>/dev/null || true
    echo ">> debootstrap $UBU_CODENAME (arm64) from $UBU_MIRROR"
    S debootstrap --arch=arm64 --variant=minbase \
      --include="bash,ca-certificates,git,curl,python3" \
      "$UBU_CODENAME" "$ROOTFS_STAGING" "$UBU_MIRROR"
  fi
fi

# If (still) empty, fall back to the raw ubuntu-base tarball (no apt packages;
# still glibc + apt so prebuilt static tools like gh/lean run fine).
if [ ! -x "$ROOTFS_STAGING/bin/sh" ] && [ ! -x "$ROOTFS_STAGING/usr/bin/dpkg" ]; then
  echo ">> Falling back to raw ubuntu-base aarch64 tarball"
  UBUNTU_REL="https://cdimage.ubuntu.com/ubuntu-base/releases/noble/release/"
  UBUNTU_ARCHIVE="$(curl -fsSL "$UBUNTU_REL" | grep -o 'ubuntu-base-[0-9.]*-base-arm64\.tar\.gz' | sort -V | uniq | tail -1)"
  [ -n "$UBUNTU_ARCHIVE" ] || { echo "ERROR: could not find ubuntu-base archive" >&2; exit 1; }
  curl -fsSL -o "$WORK/rootfs.tar.gz" "$UBUNTU_REL$UBUNTU_ARCHIVE"
  tar xzf "$WORK/rootfs.tar.gz" -C "$ROOTFS_STAGING"
fi

# Sanity: the guest must have a sh and a dynamic loader (glibc).
[ -x "$ROOTFS_STAGING/bin/sh" ] || [ -x "$ROOTFS_STAGING/usr/bin/env" ] \
  || { echo "ERROR: Ubuntu rootfs has no /bin/sh" >&2; exit 1; }

# --- 4. Emit the assets -------------------------------------------------------
# proot + loader + shmem + daemon ship as native libs (lib/*.so), NOT assets:
# the package manager extracts jniLibs with the executable bit on a
# system-blessed path; some devices refuse execve() on app-chmodded filesDir
# payloads (error=13). libtalloc.so.2 has no `.so` extension (AGP drops it from
# jniLibs), so it ships as an asset extracted to filesDir/native_deps at install.
JNILIBS="$ROOT/app/src/main/jniLibs/arm64-v8a"
mkdir -p "$ASSETS" "$JNILIBS"
echo ">> Writing $ASSETS/ubuntu-rootfs.bin (gzipped tar, .bin extension avoids AGP gunzipping)"
tar czf "$ASSETS/ubuntu-rootfs.bin" -C "$ROOTFS_STAGING" .
rm -f "$ASSETS/ubuntu-rootfs.tar.gz" "$ASSETS/proot" "$ASSETS/proot-loader" \
  "$ASSETS/forgerig-daemon"
echo ">> Writing jniLibs: libproot.so libproot_loader.so libandroid-shmem.so libforgerig_daemon.so"
cp "$PROOT_BIN" "$JNILIBS/libproot.so"
cp "$PROOT_LOADER" "$JNILIBS/libproot_loader.so"
cp "$LIBSHMEM" "$JNILIBS/libandroid-shmem.so"
cp "$DAEMON_BIN" "$JNILIBS/libforgerig_daemon.so"
cp "$LIBTALLOC" "$ASSETS/libtalloc.so.2"
chmod 755 "$JNILIBS"/libproot.so "$JNILIBS"/libproot_loader.so \
  "$JNILIBS"/libandroid-shmem.so "$JNILIBS"/libforgerig_daemon.so \
  "$ASSETS"/libtalloc.so.2
[ "$(readelf -d "$JNILIBS/libproot.so" | grep -c 'NEEDED.*libtalloc.so.2\|NEEDED.*libandroid-shmem.so')" -eq 2 ] \
  || echo "WARNING: expected proot to need libtalloc.so.2 + libandroid-shmem.so" >&2

echo ">> Done. Assets:"
ls -l "$ASSETS/ubuntu-rootfs.bin" "$ASSETS/libtalloc.so.2" \
  "$JNILIBS"/libproot.so "$JNILIBS"/libproot_loader.so \
  "$JNILIBS"/libandroid-shmem.so "$JNILIBS"/libforgerig_daemon.so