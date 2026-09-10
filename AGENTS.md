# AGENTS.md — ForgeRig contributor workflow

## Before starting ANY task: sync with latest `main`
PR branches go stale fast (automerge squashes into `main` constantly).
A branch cut from an old `main` will be CONFLICTING by the time you push.

1. `git fetch origin`
2. `git checkout main && git reset --hard origin/main`
3. Create the task branch FROM the fresh `main`:
   `git checkout -b <type>/<short-name>`
4. Before pushing / opening a PR, rebase once more:
   `git fetch origin && git rebase origin/main`
   - If commits were already merged upstream, `git rebase --skip` the
     duplicates (signing config, dead-code cleanup, progress UI, etc. —
     check `git log --oneline origin/main` first).
   - Never force-push someone else's branch; use `--force-with-lease`
     only on your own task branches.

## Before committing: verify the build
- Android unit tests (assets not required):
  `export PATH="/data/data/com.termux/files/usr/bin:$PATH" && ./gradlew :app:testDebugUnitTest -PskipContainerAssetsCheck`
- Full APK packaging requires generated container assets first:
  `export HOME="/data/data/com.termux/files/home" && bash scripts/prepare-assets.sh`
  then `./gradlew assembleDebug` (fails loudly via `checkContainerAssets`
  if `app/src/main/assets/ubuntu-rootfs.bin` or
  `app/src/main/jniLibs/arm64-v8a/libproot{,_loader}.so` or its DT_NEEDED
  libs (`app/src/main/assets/libtalloc.so.2`,
  `app/src/main/jniLibs/arm64-v8a/libandroid-shmem.so`) are missing).

## Asset pipeline gotchas (must-know)
- `scripts/prepare-assets.sh` emits `ubuntu-rootfs.bin` (gzipped tar with a
  `.bin` extension) — NOT `.tar.gz`. AGP auto-gunzips `.gz` assets at merge
  time, which renames the entry to `ubuntu-rootfs.tar` inside the APK and
  breaks `AssetManager.open("ubuntu-rootfs.tar.gz")` with FileNotFoundException.
- proot + loader + daemon ship as `app/src/main/jniLibs/arm64-v8a/` native
  libs (libproot.so, libproot_loader.so, libandroid-shmem.so,
  libforgerig_daemon.so) — NOT assets: some devices refuse execve() on
  app-chmodded filesDir payloads (error=13), so only PackageManager-extracted
  native libs run everywhere. `packaging { jniLibs { useLegacyPackaging = true } }`
  pins extraction at install.
- The daemon runs HOST-side: `ContainerService` execs `libforgerig_daemon.so`
  directly (static musl) and sets `CONTAINER_PROOT`/`CONTAINER_ROOTFS`/
  `PROOT_LOADER`/`CONTAINER_RESOLV_CONF`, so the daemon drives the work guest
  through proot. There is no `root/start.sh` entrypoint anymore.
- The work guest rootfs is Ubuntu/glibc (debootstrap'd aarch64 on the CI
  runner via qemu-user-static, with a raw ubuntu-base tarball fallback), NOT
  Alpine. `scripts/prepare-assets.sh` `--include` list is the way to add guest
  packages (bash, git, python3, …). Keep it glibc: gh, node, Lean/elan all ship
  glibc binaries.
- Ubuntu base has hard links (usr/bin/perl etc.); `AssetExtractor` recreates
  BOTH symlinks and hardlinks (its symlink handling is what fixed ENOEXEC,
  hardlink handling is what keeps perl/gunzip whole).
- Termux-built proot is dynamically linked (`DT_NEEDED libtalloc.so.2`,
  `libandroid-shmem.so`) with RUNPATH `/data/data/com.termux/...`. That dir is
  unreadable to the app UID, so `prepare-assets.sh` ships `libandroid-shmem.so`
  as a jniLib. `libtalloc.so.2` has no `.so` extension — AGP drops non-`.so`
  names from merged jniLibs — so it ships as `assets/libtalloc.so.2` and
  `AssetExtractor` extracts it to `filesDir/native_deps/`. `ContainerService`
  sets `LD_LIBRARY_PATH=nativeLibraryDir:filesDir/native_deps` before exec,
  and the daemon sets a guest `PATH=/usr/local/sbin:...:/bin` + `HOME=/root`
  per proot invocation (the host PATH is meaningless inside the guest).
  Both must ride along with any proot upgrade.
- `AssetExtractor` opens `ubuntu-rootfs.bin` first, then falls back to
  `.tar.gz` / plain `.tar` for older APKs. Keep all three in sync across
  `prepare-assets.sh`, `checkContainerAssets` (app/build.gradle.kts),
  `.gitignore`, and `AssetExtractor.ROOTFS_CANDIDATES`.
- Generated assets are gitignored; CI regenerates them via `prepare-assets.sh`
  with `FORGERIG_CARGO_TARGET=aarch64-unknown-linux-musl` and
  `FORGERIG_CARGO_FEATURES=vendored-openssl` (openssl-sys cross-build fails otherwise).

## Environment notes (Termux)
- `git`/`gh` live outside the default PATH for agents:
  `export PATH="/data/data/com.termux/files/usr/bin:$PATH"`
- `cargo`/`rustc` needs `$HOME` set plus cargo bin on PATH:
  `export HOME="/data/data/com.termux/files/home"` and add
  `/data/data/com.termux/files/home/.cargo/bin` to PATH.
- `gh` needs `HOME` set for auth (`gh auth setup-git` fails otherwise);
  push with `git push -u origin <branch>` after exporting HOME.

## Never commit secrets
`keystore.properties`, `local.properties`, and `*.keystore` stay local.
Check `git status` before every commit; only stage intended files.
