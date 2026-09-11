#!/usr/bin/env bash
# Prepare the Android container assets that the app extracts at install time.
#
# Produces:
#   dist/container/ubuntu-rootfs.bin        Ubuntu (glibc) aarch64 rootfs, zstd tar
#                                           (NOT baked into the APK; the runner
#                                           downloads it on install)
#   dist/container/container-manifest.json  integrity manifest (rootfs + optional
#                                           Lean toolchain entry with upstream URL)
#   app/src/main/assets/libtalloc.so.2      proot DT_NEEDED dep (asset; name has no .so)
#   app/src/main/jniLibs/arm64-v8a/         proot + loader + deps + the daemon
#     libproot.so, libproot_loader.so, libandroid-shmem.so, libforgerig_daemon.so
#
# NOTE: the payload keeps a `.bin` extension (not `.tar.gz`) so AGP never
# auto-gunzips it at merge time (which would rename the entry and break
# AssetManager.open). The app's AssetExtractor sniffs zstd/gzip magic bytes.
#
# The daemon runs HOST-side (a Bionic PIE binary exec'd straight from the
# app's native lib dir) and drives the work guest through proot, so it ships
# as a jniLib rather than being baked into the rootfs. Bionic (not musl-static):
# musl has no usable DNS on Android (no /etc/resolv.conf), so every daemon-side
# HTTPS call failed name resolution. Cross-build it with:
#   export FORGERIG_CARGO_TARGET=aarch64-linux-android
#   export ANDROID_NDK_HOME=/path/to/android-ndk
# (the GitHub runner does this; the script derives the NDK clang linker from
# ANDROID_NDK_HOME). Local Termux builds need no --target: the host triple is
# already aarch64-linux-android. Pass extra cargo features via
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

# --- 1. Build the daemon (host-side orchestrator, Bionic PIE) ------------------
DAEMON_TARGET="${FORGERIG_CARGO_TARGET:-}"
CARGO_FEATURES="${FORGERIG_CARGO_FEATURES:-}"
CARGO_ARGS=""
if [ -n "$CARGO_FEATURES" ]; then
  CARGO_ARGS="--features $CARGO_FEATURES"
fi
if [[ "$DAEMON_TARGET" == *android* ]]; then
  # The NDK lives at different paths per machine, so derive the cross linker
  # from ANDROID_NDK_HOME (minSdk 26 => android26 clang).
  : "${ANDROID_NDK_HOME:?set ANDROID_NDK_HOME to cross-build the daemon for Android}"
  NDK_LLVM="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin"
  export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$NDK_LLVM/aarch64-linux-android26-clang"
  export CC_aarch64_linux_android="$CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER"
  export AR_aarch64_linux_android="$NDK_LLVM/llvm-ar"
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
if [[ "$DAEMON_TARGET" == *android* ]] || [ -z "$DAEMON_TARGET" ]; then
  # The shipped daemon must be a dynamically-linked Bionic binary (NEEDED
  # libc.so): a static musl binary has no DNS on Android and can never
  # resolve daemon-side HTTPS (Lean downloads, model APIs).
  if command -v readelf >/dev/null 2>&1; then
    readelf -d "$DAEMON_BIN" | grep -q 'NEEDED.*libc\.so' \
      || { echo "ERROR: $DAEMON_BIN is not a Bionic dynamic binary (missing NEEDED libc.so)" >&2; exit 1; }
  else
    echo "WARNING: readelf unavailable; skipping Bionic linkage check" >&2
  fi
fi

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
  S apt-get install -y -qq debootstrap qemu-user-static ca-certificates zstd \
    || { echo "WARNING: could not install debootstrap/qemu-user-static/zstd; falling back to raw ubuntu-base" >&2; }
  if command -v debootstrap >/dev/null 2>&1; then
    S update-binfmts --enable qemu-aarch64 2>/dev/null || true
    echo ">> debootstrap $UBU_CODENAME (arm64) from $UBU_MIRROR"
    S debootstrap --arch=arm64 --variant=minbase --no-check-gpg \
      --include="bash,ca-certificates,git,curl,zstd" \
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

# debootstrap leaves root-owned (0600) files (etc/shadow, dev/*, dpkg locks);
# make them readable/writable by the build user so the final tar + cleanup work
# without root. Ownership in the guest is irrelevant (proot fakes -0).
if [ "$(id -u)" -ne 0 ] && command -v sudo >/dev/null 2>&1; then
  sudo chown -R "$(id -u):$(id -g)" "$ROOTFS_STAGING" 2>/dev/null || true
fi

# --- 4. Emit the bundled APK bits ---------------------------------------------
# proot + loader + shmem + daemon ship as native libs (lib/*.so), NOT assets:
# the package manager extracts jniLibs with the executable bit on a
# system-blessed path; some devices refuse execve() on app-chmodded filesDir
# payloads (error=13). libtalloc.so.2 has no `.so` extension (AGP drops it from
# jniLibs), so it ships as an asset extracted to filesDir/native_deps at install.
# The rootfs itself is NOT bundled: it lives in dist/container and is downloaded
# by the runner at install (step 5), keeping the APK slim.
JNILIBS="$ROOT/app/src/main/jniLibs/arm64-v8a"
mkdir -p "$ASSETS" "$JNILIBS"
rm -f "$ASSETS/ubuntu-rootfs.bin" "$ASSETS/ubuntu-rootfs.tar.gz" \
  "$ASSETS/ubuntu-rootfs.tar" "$ASSETS/proot" "$ASSETS/proot-loader" \
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
echo ">> Bundled APK bits: $ASSETS/libtalloc.so.2 + $JNILIBS/*"

# --- 5. Emit the swappable container payload (downloads from `container-latest`) --
# The rootfs is the heavy, frequently-changing piece, so it is published as a
# standalone GitHub release the app downloads (chunked + sha-verified) at
# install, letting it upgrade independently of the runner APK. The manifest also
# records the optional Lean toolchain (published upstream by leanprover/lean4):
# its `url` lets the host-side daemon download it on demand and install it into
# /usr/local inside the guest.
DIST="$ROOT/dist/container"
mkdir -p "$DIST"
echo ">> Writing $DIST/ubuntu-rootfs.bin (zstd tar, .bin extension avoids AGP gunzipping)"
# zstd for ~25-30% smaller payload than gzip; -T0 spreads across cores. Use GNU
# tar's --zstd if available, else pipe through zstd(1).
if tar --zstd -cf "$DIST/ubuntu-rootfs.bin" -C "$ROOTFS_STAGING" . 2>/dev/null; then
  :
else
  tar cf - -C "$ROOTFS_STAGING" . | zstd -T0 -c > "$DIST/ubuntu-rootfs.bin"
fi
ROOTFS_SHA="$(sha256sum "$DIST/ubuntu-rootfs.bin" | cut -d' ' -f1)"
ROOTFS_SIZE="$(stat -c%s "$DIST/ubuntu-rootfs.bin" 2>/dev/null || wc -c < "$DIST/ubuntu-rootfs.bin")"

# Resolve the Lean aarch64 release from the GitHub API (no local download at
# build time: size + sha256 digest come straight from the API payload).
# FORGERIG_LEAN_VERSION selects which release to follow:
#   "latest"  -> newest published release INCLUDING pre-releases (default)
#   "stable"  -> newest stable release only
#   "X.Y.Z"   -> exact pinned tag (vX.Y.Z)
LEAN_VERSION="${FORGERIG_LEAN_VERSION:-latest}"
case "$LEAN_VERSION" in
  latest) LEAN_ENDPOINT="releases?per_page=50" ;;
  stable) LEAN_ENDPOINT="releases/latest" ;;
  *)      LEAN_ENDPOINT="releases/tags/v$LEAN_VERSION" ;;
esac
LEAN_JSON="$(curl -fsSL --max-time 30 "https://api.github.com/repos/leanprover/lean4/$LEAN_ENDPOINT" 2>/dev/null || true)"

if command -v python3 >/dev/null 2>&1; then
  # The 50-release listing is ~400 KB — far over the ~128 KB argv budget
  # (E2BIG "Argument list too long", exit 126). Stage it in a temp file.
  LEAN_JSON_FILE="$(mktemp)"
  if [ -n "$LEAN_JSON" ]; then
    printf '%s' "$LEAN_JSON" > "$LEAN_JSON_FILE"
  else
    : > "$LEAN_JSON_FILE"
  fi
  python3 - "$DIST" "$ROOTFS_SHA" "$ROOTFS_SIZE" "$LEAN_JSON_FILE" <<'PY'
import json, os, sys
dist, rootfs_sha, rootfs_size, lean_json_file = sys.argv[1:]
assets = {"ubuntu-rootfs.bin": {"sha256": rootfs_sha, "size": int(rootfs_size)}}
try:
    with open(lean_json_file) as fh:
        data = json.load(fh)
    releases = data if isinstance(data, list) else [data]
    for rel in releases:
        if rel.get("draft") or not rel.get("assets"):
            continue
        for a in rel["assets"]:
            if a.get("name", "").endswith("linux_aarch64.tar.zst"):
                digest = a.get("digest") or ""
                sha = digest.split(":", 1)[1] if ":" in digest else ""
                assets[a["name"]] = {
                    "sha256": sha,
                    "size": a["size"],
                    "url": a.get("browser_download_url", ""),
                }
                break
        if any(n.endswith("linux_aarch64.tar.zst") for n in assets):
            break
except Exception as e:
    print(f"WARNING: could not parse Lean release JSON: {e}", file=sys.stderr)
finally:
    try:
        os.remove(lean_json_file)
    except OSError:
        pass
with open(f"{dist}/container-manifest.json", "w") as fh:
    json.dump({"version": 2, "assets": assets}, fh, indent=2)
    fh.write("\n")
PY
else
  echo "WARNING: python3 unavailable; manifest omits the optional Lean entry" >&2
  cat > "$DIST/container-manifest.json" <<JSON
{
  "version": 1,
  "assets": {
    "ubuntu-rootfs.bin": { "sha256": "$ROOTFS_SHA", "size": $ROOTFS_SIZE }
  }
}
JSON
fi

echo ">> Wrote $DIST/container-manifest.json"
cat "$DIST/container-manifest.json"
echo ">> Done. Bundled APK bits:"
ls -l "$ASSETS/libtalloc.so.2" \
  "$JNILIBS"/libproot.so "$JNILIBS"/libproot_loader.so \
  "$JNILIBS"/libandroid-shmem.so "$JNILIBS"/libforgerig_daemon.so
echo ">> Downloadable payload:"
ls -l "$DIST/ubuntu-rootfs.bin"