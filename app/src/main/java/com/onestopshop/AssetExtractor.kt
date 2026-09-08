package com.onestopshop

import android.content.Context
import org.apache.commons.compress.archivers.tar.TarArchiveInputStream
import java.io.File
import java.io.FileOutputStream
import java.util.zip.GZIPInputStream
import kotlin.concurrent.thread

class AssetExtractor(private val context: Context) {

    interface ProgressListener {
        fun onProgress(message: String, percentage: Int)
        fun onComplete()
        fun onError(error: String)
    }

    private var listener: ProgressListener? = null

    fun setProgressListener(listener: ProgressListener) {
        this.listener = listener
    }

    fun extractAssets() {
        thread {
            val assetManager = context.assets
            val targetDir = File(context.filesDir, "ubuntu_rootfs")

            if (!targetDir.exists()) {
                targetDir.mkdirs()
            }

            try {
                listener?.onProgress("Extracting proot binary...", 10)

                assetManager.open("proot").use { inputStream ->
                    val prootFile = File(targetDir, "proot")
                    FileOutputStream(prootFile).use { output ->
                        inputStream.copyTo(output)
                    }
                    prootFile.setExecutable(true, false)
                }

                listener?.onProgress("Proot extracted successfully", 30)
                listener?.onProgress("Extracting Ubuntu rootfs...", 40)

                assetManager.open("ubuntu-rootfs.tar.gz").use { inputStream ->
                    GZIPInputStream(inputStream).use { gzipStream ->
                        TarArchiveInputStream(gzipStream).use { tarStream ->
                            var entry = tarStream.nextTarEntry
                            var count = 0
                            val totalEntries = countTarEntries(tarStream)
                            tarStream.close()
                            assetManager.open("ubuntu-rootfs.tar.gz").use { inputStream2 ->
                                GZIPInputStream(inputStream2).use { gzipStream2 ->
                                    TarArchiveInputStream(gzipStream2).use { tarStream2 ->
                                        while (entry != null) {
                                            val targetPath = targetDir.canonicalPath
                                            val outputFile = File(targetDir, entry.name)
                                            val outputPath = outputFile.canonicalPath
                                            if (!outputPath.startsWith(targetPath + File.separator)) {
                                                throw SecurityException("Path traversal attack detected: \${entry.name}")
                                            }

                                            if (entry.isDirectory) {
                                                outputFile.mkdirs()
                                            } else {
                                                outputFile.parentFile?.mkdirs()
                                                FileOutputStream(outputFile).use { output ->
                                                    tarStream2.copyTo(output)
                                                }
                                                if ((entry.mode and 0b001_001_001) != 0) {
                                                    outputFile.setExecutable(true, false)
                                                }
                                            }
                                            count++
                                            val progress = 40 + (count * 50 / totalEntries.coerceAtLeast(1))
                                            listener?.onProgress("Extracting: ${entry.name}", progress.coerceAtMost(95))
                                            entry = tarStream2.nextTarEntry
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                listener?.onProgress("Finalizing installation...", 95)
                listener?.onComplete()
            } catch (e: Exception) {
                listener?.onError("Extraction failed: ${e.message}")
                e.printStackTrace()
            }
        }
    }

    private fun countTarEntries(tarStream: TarArchiveInputStream): Int {
        var count = 0
        var entry = tarStream.nextTarEntry
        while (entry != null) {
            count++
            entry = tarStream.nextTarEntry
        }
        return count
    }
}
