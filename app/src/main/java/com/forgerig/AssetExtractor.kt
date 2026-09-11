package com.forgerig

import android.content.ContentValues
import android.content.Context
import android.os.Build
import android.os.Environment
import android.provider.MediaStore
import android.util.Log
import org.apache.commons.compress.archivers.tar.TarArchiveInputStream
import java.io.BufferedInputStream
import java.io.File
import java.io.FileInputStream
import java.io.FileOutputStream
import java.io.IOException
import java.io.InputStream
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale
import java.util.zip.GZIPInputStream
import kotlin.concurrent.thread

interface InstallProgress {
    fun onProgress(percent: Int, stage: String, detail: String)
    fun onStep(step: Int)
    fun onError(message: String, detail: String)
    fun onDone()
}

class AssetExtractor(private val context: Context) {

    companion object {
        private const val TAG = "AssetExtractor"
        private val ROOTFS_CANDIDATES = arrayOf(
            "ubuntu-rootfs.bin",
            "ubuntu-rootfs.tar.gz",
            "ubuntu-rootfs.tar"
        )

        fun gitSha(): String {
            return try {
                BuildConfig.GIT_SHA.ifEmpty { "dev" }
            } catch (e: Exception) {
                "dev"
            }
        }

        fun sharedLogFileName(): String {
            return "forgerig-install-${BuildConfig.BUILD_TYPE}-${gitSha()}.log"
        }

        fun buildId(): String {
            return "${BuildConfig.BUILD_TYPE}-${gitSha()}"
        }

        fun resolveProotFile(context: Context): File =
            File(context.applicationInfo.nativeLibraryDir, "libproot.so")

        fun resolveLoaderFile(context: Context): File =
            File(context.applicationInfo.nativeLibraryDir, "libproot_loader.so")

        // The host-side orchestrator daemon (Bionic PIE) exec'd directly by the
        // app; it drives the work guest through proot via CONTAINER_* env.
        fun resolveDaemonFile(context: Context): File =
            File(context.applicationInfo.nativeLibraryDir, "libforgerig_daemon.so")

        // libtalloc.so.2 is a proot DT_NEEDED dep. AGP only merges jniLibs
        // files ending in `.so` (it silently drops `libtalloc.so.2`), so it
        // ships as an asset and is extracted here. Keep in sync with
        // prepare-assets.sh, checkContainerAssets, and .gitignore.
        const val TALLOC_ASSET = "libtalloc.so.2"

        // Key in container-manifest.json for the swappable Ubuntu rootfs.
        const val ROOTFS_ASSET = "ubuntu-rootfs.bin"

        fun resolveTallocFile(context: Context): File =
            File(context.filesDir, "native_deps/libtalloc.so.2")

        fun loaderSearchPath(context: Context): String {
            val libs = context.applicationInfo.nativeLibraryDir
            val deps = File(context.filesDir, "native_deps").absolutePath
            return "$libs:$deps"
        }

        // The work rootfs ships no /etc/resolv.conf, so guest tools (apt/git/
        // lean-lake) cannot resolve anything (DNS lookup failure on every API
        // call). Generate one from the device's current DNS servers (respects
        // VPN / private DNS) with public fallback, refreshed on every launch.
        // The caller bind-mounts the result over the guest's /etc/resolv.conf.
        fun writeResolvConf(context: Context): File? {
            return try {
                val servers = LinkedHashSet<String>()
                try {
                    val proc = Runtime.getRuntime().exec(arrayOf("getprop"))
                    proc.inputStream.bufferedReader().useLines { lines ->
                        lines.forEach { line ->
                            val m = Regex("""\[(?:net|dhcp)[^]]*dns\d*\]: \[(.+)]""").find(line.trim())
                            val ip = m?.groupValues?.getOrNull(1)?.trim()
                            if (!ip.isNullOrEmpty() && ip != "0.0.0.0" && ip != "::") servers.add(ip)
                        }
                    }
                    proc.waitFor()
                } catch (e: Exception) {
                    Log.w(TAG, "getprop for DNS failed: $e")
                }
                if (servers.isEmpty()) {
                    servers.add("8.8.8.8")
                    servers.add("1.1.1.1")
                }
                val file = File(context.filesDir, "resolv.conf")
                file.writeText(servers.joinToString("\n", postfix = "\n") { "nameserver $it" })
                logShared(context, "resolv.conf: ${servers.joinToString(",")}")
                file
            } catch (e: Exception) {
                Log.w(TAG, "writeResolvConf failed: $e")
                null
            }
        }

        fun deviceInfo(): String {
            return "Android ${Build.VERSION.RELEASE} (SDK ${Build.VERSION.SDK_INT})"
        }

        fun describeFile(file: File): String {
            return try {
                "${file.absolutePath} exists=${file.exists()} size=${if (file.exists()) file.length() else -1} " +
                    "readable=${file.canRead()} executable=${file.canExecute()} " +
                    "parentWritable=${file.parentFile?.canWrite()}"
            } catch (e: Exception) {
                "${file.absolutePath} stat failed: ${e.message}"
            }
        }

        fun ensureExecutable(file: File): String? {
            val setOk = try {
                file.setExecutable(true, false)
            } catch (e: Exception) {
                false
            }
            if (file.canExecute()) {
                return null
            }
            val chmodResult = try {
                android.system.Os.chmod(file.absolutePath, 493)
                "ok"
            } catch (e: android.system.ErrnoException) {
                "errno=${e.errno} ${e.message}"
            } catch (e: Exception) {
                e.message ?: e.toString()
            }
            return if (file.canExecute()) {
                null
            } else {
                "not executable (setExecutable=$setOk, chmod=$chmodResult) [${describeFile(file)}] [${deviceInfo()}]"
            }
        }

        fun logShared(context: Context, message: String) {
            Log.i(TAG, message)
            val fileName = sharedLogFileName()
            try {
                val stamp = SimpleDateFormat("yyyy-MM-dd HH:mm:ss.SSS", Locale.US).format(Date())
                val line = "[$stamp] $message\n"
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                    val resolver = context.contentResolver
                    var uri = LogUriCache.uri ?: resolveLogUri(resolver, fileName)?.also { LogUriCache.uri = it }
                    try {
                        if (uri != null) {
                            resolver.openOutputStream(uri, "wa")?.use { it.write(line.toByteArray()) }
                        }
                    } catch (e: Exception) {
                        LogUriCache.uri = null
                        uri = resolveLogUri(resolver, fileName)?.also { LogUriCache.uri = it }
                        if (uri != null) {
                            resolver.openOutputStream(uri, "wa")?.use { it.write(line.toByteArray()) }
                        }
                    }
                } else {
                    @Suppress("DEPRECATION")
                    val downloadsDir = Environment.getExternalStoragePublicDirectory(Environment.DIRECTORY_DOWNLOADS)
                    if (!downloadsDir.exists()) {
                        downloadsDir.mkdirs()
                    }
                    FileOutputStream(File(downloadsDir, fileName), true).use { it.write(line.toByteArray()) }
                }
            } catch (e: Exception) {
                // A failure to append to the shared log is itself an error: it
                // must stay visible somewhere, so escalate to Log.e (the app's
                // logcat is the last-resort sink when Downloads storage fails).
                Log.e(TAG, "writeSharedLog failed for '$fileName': ${e.message}")
            }
        }

        private fun resolveLogUri(resolver: android.content.ContentResolver, fileName: String): android.net.Uri? {
            val existing = resolver.query(
                MediaStore.Downloads.EXTERNAL_CONTENT_URI,
                arrayOf(MediaStore.Downloads._ID),
                "${MediaStore.Downloads.DISPLAY_NAME}=?",
                arrayOf(fileName),
                null
            )
            return if (existing != null && existing.moveToFirst()) {
                val id = existing.getLong(0)
                existing.close()
                android.net.Uri.withAppendedPath(MediaStore.Downloads.EXTERNAL_CONTENT_URI, id.toString())
            } else {
                existing?.close()
                val values = ContentValues().apply {
                    put(MediaStore.Downloads.DISPLAY_NAME, fileName)
                    put(MediaStore.Downloads.MIME_TYPE, "text/plain")
                    put(MediaStore.Downloads.RELATIVE_PATH, Environment.DIRECTORY_DOWNLOADS)
                }
                resolver.insert(MediaStore.Downloads.EXTERNAL_CONTENT_URI, values)
            }
        }

        private object LogUriCache {
            @Volatile
            var uri: android.net.Uri? = null
        }
    }

    private var progress: InstallProgress = object : InstallProgress {
        override fun onProgress(percent: Int, stage: String, detail: String) {}
        override fun onStep(step: Int) {}
        override fun onError(message: String, detail: String) {}
        override fun onDone() {}
    }

    fun setProgressListener(listener: InstallProgress): AssetExtractor {
        progress = listener
        return this
    }

    private fun log(message: String) {
        logShared(context, message)
    }

    private fun listBundledAssets(): String {
        return try {
            (context.assets.list("")?.sorted()?.joinToString(", ") ?: "<unlistable>")
        } catch (e: Exception) {
            "<list failed: ${e.message}>"
        }
    }

    private enum class Compression { NONE, GZIP, ZSTD }

    private fun compressionOf(stream: InputStream): Pair<InputStream, Compression> {
        val bis = if (stream is BufferedInputStream) stream else BufferedInputStream(stream)
        bis.mark(4)
        val magic = ByteArray(4)
        val n = bis.read(magic)
        bis.reset()
        val comp = when {
            n >= 4 && magic[0] == 0x28.toByte() && magic[1] == 0xB5.toByte() &&
                magic[2] == 0x2F.toByte() && magic[3] == 0xFD.toByte() -> Compression.ZSTD
            n >= 2 && magic[0] == 0x1F.toByte() && magic[1] == 0x8B.toByte() -> Compression.GZIP
            else -> Compression.NONE
        }
        return bis to comp
    }

    private data class RootfsStream(
        val stream: InputStream,
        val compression: Compression,
        val assetName: String,
        val totalBytes: Long,
    ) : java.io.Closeable {
        override fun close() = stream.close()
    }

    private fun openRootfs(): RootfsStream {
        // Online-first: fetch the swappable container payload (slim, upgradable
        // APK). Fall back to the bundled asset so full/offline APKs still work.
        try {
            val manifest = ContainerAssets.fetchManifest(context)
            val mib = manifest[ROOTFS_ASSET]?.size ?: 0L
            val file = ContainerAssets.ensure(context, ROOTFS_ASSET, manifest) { bytes ->
                if (mib > 0) {
                    val pct = (2 + 5 * bytes / mib).toInt().coerceIn(2, 7)
                    progress.onProgress(
                        pct,
                        "Downloading container payload…",
                        "${bytes / (1024 * 1024)} / ${mib / (1024 * 1024)} MB"
                    )
                }
            }
            log("Rootfs from container download: ${file.absolutePath} (${file.length()} bytes)")
            val (stream, comp) = compressionOf(FileInputStream(file))
            return RootfsStream(stream, comp, file.absolutePath, file.length())
        } catch (e: Exception) {
            log("Container download unavailable (${e.message}); falling back to bundled rootfs")
        }
        var lastError: Exception? = null
        for (name in ROOTFS_CANDIDATES) {
            try {
                val (stream, comp) = compressionOf(context.assets.open(name))
                return RootfsStream(stream, comp, name, assetLength(name))
            } catch (e: IOException) {
                lastError = e
            }
        }
        throw IOException(
            "Rootfs archive unavailable (download failed and no bundled asset: " +
                "${ROOTFS_CANDIDATES.joinToString(", ")}). " +
                "Bundled assets: [${listBundledAssets()}]. Last error: ${lastError?.message}"
        )
    }

    fun extractAssets() {
        thread {
            try {
                log("===== Install started (build=${buildId()}, log=${sharedLogFileName()}) =====")
                val targetDir = File(context.filesDir, "ubuntu_rootfs")
                if (!targetDir.exists()) {
                    targetDir.mkdirs()
                }

                progress.onStep(0)
                progress.onProgress(2, "Preparing runtime…", "Verifying proot")
                log("PROGRESS [2%] Preparing runtime… - Verifying proot")
                val prootFile = resolveProotFile(context)
                if (!prootFile.exists() || prootFile.length() == 0L || !isElf(prootFile)) {
                    throw IOException("proot native library is missing or invalid: ${describeFile(prootFile)} (broken APK build?)")
                }
                ensureExecutable(prootFile)?.let {
                    throw IOException("proot native library is $it")
                }
                val loaderFile = resolveLoaderFile(context)
                if (!loaderFile.exists() || loaderFile.length() == 0L || !isElf(loaderFile)) {
                    throw IOException("proot loader native library is missing or invalid: ${describeFile(loaderFile)} (broken APK build?)")
                }
                ensureExecutable(loaderFile)?.let {
                    throw IOException("proot loader native library is $it")
                }
                val daemonFile = resolveDaemonFile(context)
                if (!daemonFile.exists() || daemonFile.length() == 0L || !isElf(daemonFile)) {
                    throw IOException("orchestrator daemon native library is missing or invalid: ${describeFile(daemonFile)} (broken APK build?)")
                }
                ensureExecutable(daemonFile)?.let {
                    throw IOException("orchestrator daemon native library is $it")
                }
                log("Proot verified (${prootFile.absolutePath}, ${prootFile.length()} bytes, executable)")

                // proot links against libtalloc.so.2 at runtime; ship it out of
                // assets into a filesDir dir the app UID can read (the DT_NEEDED
                // RUNPATH points at /data/data/com.termux/... which is not).
                val tallocFile = resolveTallocFile(context)
                if (!tallocFile.exists() || tallocFile.length() == 0L) {
                    tallocFile.parentFile?.mkdirs()
                    context.assets.open(TALLOC_ASSET).use { input ->
                        tallocFile.outputStream().use { it.write(input.readBytes()) }
                    }
                    tallocFile.setReadable(true, false)
                    log("Extracted ${TALLOC_ASSET} to ${tallocFile.absolutePath} (${tallocFile.length()} bytes)")
                } else {
                    log("${TALLOC_ASSET} already present at ${tallocFile.absolutePath} (${tallocFile.length()} bytes)")
                }

                progress.onStep(1)
                progress.onProgress(8, "Unpacking container files…", "Starting extraction")
                log("PROGRESS [8%] Unpacking container files… - Starting extraction")

                var done = 0
                var symlinks = 0
                // Preflight the zstd native library BEFORE any decompression: a
                // missing/unloadable libzstd-jni surfaces as UnsatisfiedLinkError
                // (an Error that catch(Exception) would silently swallow and crash
                // the app with). Convert it into a visible install failure.
                try {
                    com.github.luben.zstd.util.Native.load()
                } catch (t: Throwable) {
                    throw IOException("zstd native library (libzstd-jni) failed to load: $t")
                }
                openRootfs().use { rootfs ->
                    // Single pass: byte-based progress from the (compressed)
                    // source size, so the archive is decompressed/parsed once.
                    val totalBytes = rootfs.totalBytes
                    log("Rootfs: ${rootfs.assetName} (${if (totalBytes > 0) "$totalBytes bytes" else "size unknown"})")
                    val counting = CountingInputStream(rootfs.stream)
                    val decompressed: InputStream = when (rootfs.compression) {
                        Compression.ZSTD -> org.apache.commons.compress.compressors.zstandard.ZstdCompressorInputStream(counting)
                        Compression.GZIP -> GZIPInputStream(counting)
                        Compression.NONE -> counting
                    }
                    val tarStream = TarArchiveInputStream(decompressed)
                    tarStream.use {
                        var entry = tarStream.nextTarEntry
                        while (entry != null) {
                            val outputFile = File(targetDir, entry.name)

                            // Lexical path-traversal guard. Do NOT use
                            // canonicalPath: the Ubuntu rootfs ships symlinks
                            // like /dev/stderr -> fd/2 and /dev/fd -> /proc/self/fd
                            // that canonicalize onto the HOST /proc, which would
                            // false-positive as an escape. normalize() collapses
                            // "." / ".." without resolving symlinks.
                            val norm = try {
                                java.nio.file.Paths.get(entry.name).normalize()
                            } catch (e: Exception) {
                                throw SecurityException("Invalid path in archive: ${entry.name}")
                            }
                            if (norm.isAbsolute ||
                                (norm.nameCount > 0 && norm.getName(0).toString() == "..")
                            ) {
                                throw SecurityException("Path traversal attack detected: ${entry.name}")
                            }

                            if (norm.nameCount == 0) {
                                targetDir.mkdirs()
                                done++
                                entry = tarStream.nextTarEntry
                                continue
                            }

                            if (entry.isDirectory) {
                                outputFile.mkdirs()
                            } else if (entry.isSymbolicLink) {
                                // Recreate symlinks verbatim; guest-absolute and
                                // relative targets resolve inside the proot guest
                                // at runtime. Writing them as regular files (the
                                // old behavior) leaves empty files behind, so the
                                // script interpreter (/bin/sh -> /bin/busybox)
                                // cannot run and proot dies with ENOEXEC.
                                //
                                // Tar order is unpredictable: a symlink entry like
                                // `./lib` (-> usr/lib) often arrives AFTER directory
                                // entries `./lib/aarch64-linux-gnu/`, which have
                                // already mkdir'd the target path. delete() cannot
                                // remove a non-empty dir, so Os.symlink would throw
                                // EEXIST — clear whatever's there first (mirrors
                                // GNU tar).
                                outputFile.parentFile?.mkdirs()
                                prepareTarget(outputFile)
                                try {
                                    android.system.Os.symlink(entry.linkName, outputFile.absolutePath)
                                    symlinks++
                                } catch (e: Exception) {
                                    throw IOException("Cannot create symlink ${entry.name} -> ${entry.linkName}: $e")
                                }
                            } else if (entry.isLink) {
                                // Hardlink (Ubuntu rootfs uses these, e.g. usr/bin/perl).
                                // Reference the earlier member by hardlink, or copy its
                                // content as a fallback if createLink is unavailable.
                                outputFile.parentFile?.mkdirs()
                                prepareTarget(outputFile)
                                try {
                                    java.nio.file.Files.createLink(
                                        outputFile.toPath(),
                                        File(targetDir, entry.linkName).toPath(),
                                    )
                                    symlinks++
                                } catch (e: Exception) {
                                    val src = File(targetDir, entry.linkName)
                                    if (src.exists()) {
                                        src.copyTo(outputFile, overwrite = true)
                                    } else {
                                        throw IOException("Hardlink target missing: ${entry.linkName}")
                                    }
                                }
                            } else {
                                outputFile.parentFile?.mkdirs()
                                prepareTarget(outputFile)
                                FileOutputStream(outputFile).use { output ->
                                    tarStream.copyTo(output)
                                }

                                if ((entry.mode and 0b001_001_001) != 0) {
                                    outputFile.setExecutable(true, false)
                                }
                            }

                            done++
                            val percent = if (totalBytes > 0) {
                                Math.min(98, (8 + 90 * counting.bytesRead / totalBytes).toInt())
                            } else {
                                8
                            }
                            val detail = String.format("%d files (%s)", done, entry.name)
                            progress.onProgress(percent, "Unpacking container files…", detail)
                            if (done % 500 == 0) {
                                log("PROGRESS [$percent%] Unpacking container files… - $detail")
                            }
                            entry = tarStream.nextTarEntry
                        }
                    }
                }
                if (done == 0) {
                    throw IOException("Bundled rootfs archive is empty")
                }

                progress.onStep(2)
                progress.onProgress(100, "Environment ready", "")
                log("DONE: Environment ready ($done files, $symlinks symlinks)")
                progress.onDone()
            } catch (t: Throwable) {
                // Throwable, not Exception: UnsatisfiedLinkError/OutOfMemoryError
                // (and their kin) must surface as a readable install failure in
                // the UI + shared log instead of killing the process.
                Log.e(TAG, "Installation failed", t)
                log("ERROR: Installation failed | ${t}\n${t.stackTraceToString()}")
                progress.onError("Installation failed", t.toString())
            }
        }
    }

    private class CountingInputStream(wrapped: InputStream) : java.io.FilterInputStream(wrapped) {
        @Volatile
        var bytesRead: Long = 0
            private set

        override fun read(): Int {
            val b = super.read()
            if (b >= 0) bytesRead++
            return b
        }

        override fun read(b: ByteArray, off: Int, len: Int): Int {
            val n = super.read(b, off, len)
            if (n > 0) bytesRead += n
            return n
        }
    }

    // Clear whatever sits at `path` so a following symlink/hardlink/file can be
    // created there. Unlike delete(), this also removes non-empty directories
    // (tar order can mkdir ./lib/aarch64-linux-gnu/ before the ./lib symlink
    // entry arrives). Symlinks are matched via Files.isSymbolicLink, which does
    // not follow the link, so dangling links are handled too.
    private fun prepareTarget(path: File) {
        val p = path.toPath()
        when {
            java.nio.file.Files.isSymbolicLink(p) -> java.nio.file.Files.delete(p)
            path.isDirectory -> path.deleteRecursively()
            path.exists() -> path.delete()
        }
    }

    private fun assetLength(name: String): Long {
        return try {
            context.assets.openFd(name).use { it.length }
        } catch (e: Exception) {
            -1L
        }
    }

    private fun isElf(file: File): Boolean {
        FileInputStream(file).use { input ->
            val magic = ByteArray(4)
            if (input.read(magic) != 4) return false
            return magic[0] == 0x7f.toByte() && magic[1] == 'E'.code.toByte() &&
                    magic[2] == 'L'.code.toByte() && magic[3] == 'F'.code.toByte()
        }
    }
}
