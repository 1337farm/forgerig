package com.onestopshop

import android.os.Environment
import java.io.FileWriter
import java.io.PrintWriter
import android.content.Context
import org.apache.commons.compress.archivers.tar.TarArchiveInputStream
import java.io.File
import java.io.FileInputStream
import java.io.FileOutputStream
import java.io.IOException
import java.util.zip.GZIPInputStream
import kotlin.concurrent.thread

interface InstallProgress {
    fun onProgress(percent: Int, stage: String, detail: String)
    fun onError(message: String, detail: String)
    fun onDone()
}

class AssetExtractor(private val context: Context) {

    private fun writeLog(message: String) {
        try {
            val downloadsDir = Environment.getExternalStoragePublicDirectory(Environment.DIRECTORY_DOWNLOADS)
            if (!downloadsDir.exists()) {
                downloadsDir.mkdirs()
            }
            val logFile = File(downloadsDir, "forgerig-install-${BuildConfig.BUILD_TYPE}-2b4076fcdc.log")
            FileWriter(logFile, true).use { writer ->
                writer.append("$message\n")
            }
        } catch (e: Exception) {
            e.printStackTrace()
        }
    }

    private var progress: InstallProgress = object : InstallProgress {
        override fun onProgress(percent: Int, stage: String, detail: String) {
            writeLog("PROGRESS [$percent%] $stage - $detail")
        }
        override fun onError(message: String, detail: String) {
            writeLog("ERROR: $message | $detail")
        }
        override fun onDone() {
            writeLog("DONE: Environment ready")
        }
    }

    fun setProgressListener(listener: InstallProgress): AssetExtractor {
        progress = listener
        return this
    }

    fun extractAssets() {
        thread {
            try {
                val targetDir = File(context.filesDir, "ubuntu_rootfs")
                if (!targetDir.exists()) {
                    targetDir.mkdirs()
                }

                // Stage 1: proot binary + its loader companion
                progress.onProgress(2, "Preparing runtime…", "Copying proot")
                val prootFile = File(targetDir, "proot")
                context.assets.open("proot").use { inputStream ->
                    FileOutputStream(prootFile).use { output ->
                        inputStream.copyTo(output)
                    }
                }
                prootFile.setExecutable(true, false)
                val prootSize = prootFile.length()
                if (prootSize == 0L) {
                    throw IOException("Bundled proot binary is empty")
                } else if (prootSize < 4 || !isElf(prootFile)) {
                    throw IOException("Bundled proot is not a valid ELF binary")
                }

                // Termux proot locates its loader relative to the binary
                // ($PREFIX/libexec/proot/loader), so it must be extracted as a
                // sibling in the same layout or the container cannot start.
                val loaderFile = File(targetDir, "libexec/proot/loader")
                loaderFile.parentFile?.mkdirs()
                context.assets.open("proot-loader").use { inputStream ->
                    FileOutputStream(loaderFile).use { output ->
                        inputStream.copyTo(output)
                    }
                }
                loaderFile.setExecutable(true, false)
                if (loaderFile.length() == 0L || !isElf(loaderFile)) {
                    throw IOException("Bundled proot loader is empty or not an ELF binary")
                }

                // Stage 2: ubuntu rootfs (two-pass so we can show accurate progress)
                progress.onProgress(8, "Unpacking container files…", "Reading archive")
                val totalEntries = countEntries()
                if (totalEntries == 0) {
                    throw IOException("Bundled rootfs archive is empty")
                }

                var done = 0
                context.assets.open("ubuntu-rootfs.tar.gz").use { inputStream ->
                    GZIPInputStream(inputStream).use { gzipStream ->
                        TarArchiveInputStream(gzipStream).use { tarStream ->
                            var entry = tarStream.nextTarEntry
                            while (entry != null) {
                                val outputFile = File(targetDir, entry.name)

                                // Mitigate Zip Slip vulnerability
                                if (!outputFile.canonicalPath.startsWith(targetDir.canonicalPath + File.separator)) {
                                    throw SecurityException("Path traversal attack detected: ${entry.name}")
                                }

                                if (entry.isDirectory) {
                                    outputFile.mkdirs()
                                } else {
                                    outputFile.parentFile?.mkdirs()
                                    FileOutputStream(outputFile).use { output ->
                                        tarStream.copyTo(output)
                                    }

                                    // Set executable permissions for container binaries based on tar entry mode
                                    if ((entry.mode and 0b001_001_001) != 0) { // Check for execute bit (owner, group, or other)
                                        outputFile.setExecutable(true, false)
                                    }
                                }

                                done++
                                val percent = Math.min(98, 8 + (done * 90) / totalEntries)
                                progress.onProgress(
                                    percent,
                                    "Unpacking container files…",
                                    String.format("%d / %d files (%s)", done, totalEntries, entry.name)
                                )
                                entry = tarStream.nextTarEntry
                            }
                        }
                    }
                }

                progress.onProgress(100, "Environment ready", "")
                progress.onDone()
            } catch (e: Exception) {
                progress.onError("Installation failed", e.toString())
            }
        }
    }

    private fun countEntries(): Int {
        var count = 0
        context.assets.open("ubuntu-rootfs.tar.gz").use { inputStream ->
            GZIPInputStream(inputStream).use { gzipStream ->
                TarArchiveInputStream(gzipStream).use { tarStream ->
                    var entry = tarStream.nextTarEntry
                    while (entry != null) {
                        count++
                        entry = tarStream.nextTarEntry
                    }
                }
            }
        }
        return count
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