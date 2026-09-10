package com.onestopshop

import android.content.ContentValues
import android.content.Context
import android.os.Build
import android.os.Environment
import android.provider.MediaStore
import android.util.Log
import org.apache.commons.compress.archivers.tar.TarArchiveInputStream
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

        // libtalloc.so.2 is a proot DT_NEEDED dep. AGP only merges jniLibs
        // files ending in `.so` (it silently drops `libtalloc.so.2`), so it
        // ships as an asset and is extracted here. Keep in sync with
        // prepare-assets.sh, checkContainerAssets, and .gitignore.
        const val TALLOC_ASSET = "libtalloc.so.2"

        fun resolveTallocFile(context: Context): File =
            File(context.filesDir, "native_deps/libtalloc.so.2")

        fun loaderSearchPath(context: Context): String {
            val libs = context.applicationInfo.nativeLibraryDir
            val deps = File(context.filesDir, "native_deps").absolutePath
            return "$libs:$deps"
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
            try {
                val stamp = SimpleDateFormat("yyyy-MM-dd HH:mm:ss.SSS", Locale.US).format(Date())
                val line = "[$stamp] $message\n"
                val fileName = sharedLogFileName()
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
                Log.w(TAG, "writeSharedLog failed: ${e.message}")
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

    private data class RootfsStream(val stream: InputStream, val gzipped: Boolean, val assetName: String) : java.io.Closeable {
        override fun close() = stream.close()
    }

    private fun openRootfs(): RootfsStream {
        var lastError: Exception? = null
        for (name in ROOTFS_CANDIDATES) {
            try {
                val stream = context.assets.open(name)
                val gzipped = name.endsWith(".gz") || name.endsWith(".bin")
                return RootfsStream(stream, gzipped, name)
            } catch (e: IOException) {
                lastError = e
            }
        }
        throw IOException(
            "Bundled rootfs archive is missing (tried ${ROOTFS_CANDIDATES.joinToString(", ")}). " +
                "Bundled assets: [${listBundledAssets()}]. " +
                "Last error: ${lastError?.message}"
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
                openRootfs().use { rootfs ->
                    // Single pass: byte-based progress from the compressed size,
                    // so the archive is gunzipped/parsed exactly once.
                    val totalBytes = assetLength(rootfs.assetName)
                    log("Rootfs asset: ${rootfs.assetName} (${if (totalBytes > 0) "$totalBytes bytes" else "size unknown"})")
                    val counting = CountingInputStream(rootfs.stream)
                    val tarStream = if (rootfs.gzipped) {
                        TarArchiveInputStream(GZIPInputStream(counting))
                    } else {
                        TarArchiveInputStream(counting)
                    }
                    tarStream.use {
                        var entry = tarStream.nextTarEntry
                        val canonicalTarget = targetDir.canonicalPath
                        while (entry != null) {
                            val outputFile = File(targetDir, entry.name)
                            val outputPath = outputFile.canonicalPath

                            if (outputPath != canonicalTarget &&
                                !outputPath.startsWith(canonicalTarget + File.separator)) {
                                throw SecurityException("Path traversal attack detected: ${entry.name}")
                            }

                            if (outputPath == canonicalTarget) {
                                targetDir.mkdirs()
                                done++
                                entry = tarStream.nextTarEntry
                                continue
                            }

                            if (entry.isDirectory) {
                                outputFile.mkdirs()
                            } else {
                                outputFile.parentFile?.mkdirs()
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
                log("DONE: Environment ready ($done files)")
                progress.onDone()
            } catch (e: Exception) {
                Log.e(TAG, "Installation failed", e)
                log("ERROR: Installation failed | ${e}")
                progress.onError("Installation failed", e.toString())
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
