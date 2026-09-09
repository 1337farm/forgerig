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

        fun sharedLogFileName(): String {
            return "forgerig-install.log"
        }

        fun buildId(): String {
            val sha = try {
                BuildConfig.GIT_SHA.ifEmpty { "dev" }
            } catch (e: Exception) {
                "dev"
            }
            return "${BuildConfig.BUILD_TYPE}-$sha"
        }
    }

    @Volatile
    private var cachedLogUri: android.net.Uri? = null

    private var progress: InstallProgress = object : InstallProgress {
        override fun onProgress(percent: Int, stage: String, detail: String) {}
        override fun onError(message: String, detail: String) {}
        override fun onDone() {}
    }

    fun setProgressListener(listener: InstallProgress): AssetExtractor {
        progress = listener
        return this
    }

    private fun log(message: String) {
        Log.i(TAG, message)
        writeSharedLog(message)
    }

    private fun resolveLogUri(fileName: String): android.net.Uri? {
        val resolver = context.contentResolver
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

    private fun appendToUri(uri: android.net.Uri, bytes: ByteArray) {
        context.contentResolver.openOutputStream(uri, "wa")?.use { it.write(bytes) }
    }

    private fun writeSharedLog(message: String) {
        try {
            val stamp = SimpleDateFormat("yyyy-MM-dd HH:mm:ss.SSS", Locale.US).format(Date())
            val line = "[$stamp] $message\n"
            val fileName = sharedLogFileName()
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                var uri = cachedLogUri ?: resolveLogUri(fileName)?.also { cachedLogUri = it }
                try {
                    if (uri != null) {
                        appendToUri(uri, line.toByteArray())
                    }
                } catch (e: Exception) {
                    cachedLogUri = null
                    uri = resolveLogUri(fileName)?.also { cachedLogUri = it }
                    if (uri != null) {
                        appendToUri(uri, line.toByteArray())
                    }
                }
            } else {
                @Suppress("DEPRECATION")
                val downloadsDir = Environment.getExternalStoragePublicDirectory(Environment.DIRECTORY_DOWNLOADS)
                if (!downloadsDir.exists()) {
                    downloadsDir.mkdirs()
                }
                val logFile = File(downloadsDir, fileName)
                FileOutputStream(logFile, true).use { it.write(line.toByteArray()) }
            }
        } catch (e: Exception) {
            Log.w(TAG, "writeSharedLog failed: ${e.message}")
        }
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

    private fun tarStreamFor(rootfs: RootfsStream): TarArchiveInputStream {
        return if (rootfs.gzipped) {
            TarArchiveInputStream(GZIPInputStream(rootfs.stream))
        } else {
            TarArchiveInputStream(rootfs.stream)
        }
    }

    fun extractAssets() {
        thread {
            try {
                log("===== Install started (build=${buildId()}, log=${sharedLogFileName()}) =====")
                val targetDir = File(context.filesDir, "ubuntu_rootfs")
                if (!targetDir.exists()) {
                    targetDir.mkdirs()
                }

                progress.onProgress(2, "Preparing runtime…", "Copying proot")
                log("PROGRESS [2%] Preparing runtime… - Copying proot")
                val prootFile = File(targetDir, "proot")
                try {
                    context.assets.open("proot").use { inputStream ->
                        FileOutputStream(prootFile).use { output ->
                            inputStream.copyTo(output)
                        }
                    }
                } catch (e: IOException) {
                    throw IOException("Bundled proot binary is missing. Bundled assets: [${listBundledAssets()}]. ${e.message}")
                }
                prootFile.setExecutable(true, false)
                val prootSize = prootFile.length()
                if (prootSize == 0L) {
                    throw IOException("Bundled proot binary is empty")
                } else if (prootSize < 4 || !isElf(prootFile)) {
                    throw IOException("Bundled proot is not a valid ELF binary")
                }
                log("Proot extracted ($prootSize bytes)")

                val loaderFile = File(targetDir, "libexec/proot/loader")
                loaderFile.parentFile?.mkdirs()
                try {
                    context.assets.open("proot-loader").use { inputStream ->
                        FileOutputStream(loaderFile).use { output ->
                            inputStream.copyTo(output)
                        }
                    }
                } catch (e: IOException) {
                    throw IOException("Bundled proot loader is missing. Bundled assets: [${listBundledAssets()}]. ${e.message}")
                }
                loaderFile.setExecutable(true, false)
                if (loaderFile.length() == 0L || !isElf(loaderFile)) {
                    throw IOException("Bundled proot loader is empty or not an ELF binary")
                }

                progress.onProgress(8, "Unpacking container files…", "Reading archive")
                log("PROGRESS [8%] Unpacking container files… - Reading archive")
                val counted = countEntries()
                val totalEntries = counted.total
                log("Rootfs asset: ${counted.assetName} (entries=$totalEntries)")
                if (totalEntries == 0) {
                    throw IOException("Bundled rootfs archive is empty")
                }

                var done = 0
                openRootfs().use { rootfs ->
                    tarStreamFor(rootfs).use { tarStream ->
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
                            val percent = Math.min(98, 8 + (done * 90) / totalEntries)
                            val detail = String.format("%d / %d files (%s)", done, totalEntries, entry.name)
                            progress.onProgress(percent, "Unpacking container files…", detail)
                            if (done % 500 == 0 || done == totalEntries) {
                                log("PROGRESS [$percent%] Unpacking container files… - $detail")
                            }
                            entry = tarStream.nextTarEntry
                        }
                    }
                }

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

    private data class CountResult(val total: Int, val assetName: String)

    private fun countEntries(): CountResult {
        openRootfs().use { rootfs ->
            tarStreamFor(rootfs).use { tarStream ->
                var count = 0
                var entry = tarStream.nextTarEntry
                while (entry != null) {
                    count++
                    entry = tarStream.nextTarEntry
                }
                return CountResult(count, rootfs.assetName)
            }
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
