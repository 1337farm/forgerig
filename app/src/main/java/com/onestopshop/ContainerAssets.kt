package com.onestopshop

import android.content.Context
import android.util.Log
import org.json.JSONObject
import java.io.File
import java.io.RandomAccessFile
import java.net.HttpURLConnection
import java.net.URL
import java.security.MessageDigest
import java.util.concurrent.CountDownLatch
import java.util.concurrent.Executors
import kotlin.math.min

/**
 * Download + cache the swappable container payload (Ubuntu rootfs, and later
 * the optional Lean toolchain) from the `container-latest` GitHub release.
 *
 * Each asset is described by `container-manifest.json` (name -> sha256, size)
 * and fetched with ranged (chunked, parallel) requests, re-assembled, and
 * sha256-verified before use. Verified assets are cached in filesDir/container
 * so reinstalls don't redownload until the manifest version changes.
 */
object ContainerAssets {
    private const val TAG = "ContainerAssets"

    // Release asset base URL. GitHub's asset CDN honors Range requests, which
    // is what lets us parallelize large payloads.
    const val BASE_URL =
        "https://github.com/1337farm/forgerig/releases/download/container-latest"

    private const val CHUNK = 4L * 1024 * 1024   // 4 MiB per range
    private const val PARALLEL = 4               // concurrent ranges

    data class Asset(val name: String, val sha256: String, val size: Long)

    fun cacheDir(context: Context): File =
        File(context.filesDir, "container").also { it.mkdirs() }

    fun assetFile(context: Context, name: String): File =
        File(cacheDir(context), name)

    fun fetchManifest(): Map<String, Asset> {
        val conn = (URL("$BASE_URL/container-manifest.json").openConnection() as HttpURLConnection).apply {
            connectTimeout = 15_000
            readTimeout = 20_000
        }
        try {
            return conn.inputStream.bufferedReader().use { r -> parseManifest(r.readText()) }
        } finally {
            conn.disconnect()
        }
    }

    fun parseManifest(text: String): Map<String, Asset> {
        val assets = JSONObject(text).getJSONObject("assets")
        val map = mutableMapOf<String, Asset>()
        for (key in assets.keys()) {
            val a = assets.getJSONObject(key)
            map[key] = Asset(key, a.getString("sha256"), a.getLong("size"))
        }
        return map
    }

    /**
     * Return a verified asset file: a valid cached copy is used if present,
     * otherwise it is downloaded (chunked + parallel) and sha-verified.
     */
    fun ensure(context: Context, name: String, manifest: Map<String, Asset>): File {
        val info = manifest[name] ?: throw IllegalStateException("no manifest entry for $name")
        val file = assetFile(context, name)
        if (isValid(file, info)) return file
        download(name, info, file)
        if (!isValid(file, info)) throw IllegalStateException("download failed sha256 for $name")
        return file
    }

    private fun isValid(file: File, info: Asset): Boolean =
        file.exists() && file.length() == info.size && file.sha256() == info.sha256

    private fun File.sha256(): String {
        val md = MessageDigest.getInstance("SHA-256")
        inputStream().use { ins ->
            val buf = ByteArray(64 * 1024)
            var n = ins.read(buf)
            while (n > 0) {
                md.update(buf, 0, n)
                n = ins.read(buf)
            }
        }
        return md.digest().joinToString("") { "%02x".format(it.toInt() and 0xff) }
    }

    private fun download(name: String, info: Asset, out: File) {
        out.parentFile?.mkdirs()
        val part = File(out.path + ".part")
        val nChunks = ((info.size + CHUNK - 1) / CHUNK).toInt().coerceAtLeast(1)
        Log.i(TAG, "Downloading $name (${info.size} bytes, $nChunks chunks)")

        val pool = Executors.newFixedThreadPool(PARALLEL)
        val latch = CountDownLatch(nChunks)
        val raf = RandomAccessFile(part, "rw").apply { setLength(info.size) }
        val failures = java.util.Collections.synchronizedList(mutableListOf<String>())

        for (i in 0 until nChunks) {
            val start = i * CHUNK
            val end = min(info.size - 1, start + CHUNK - 1)
            pool.execute {
                try {
                    if (end >= start) {
                        val conn = (URL("$BASE_URL/$name").openConnection() as HttpURLConnection).apply {
                            connectTimeout = 15_000
                            readTimeout = 60_000
                            setRequestProperty("Range", "bytes=$start-$end")
                        }
                        try {
                            if (conn.responseCode != 206 && conn.responseCode != 200) {
                                throw IllegalStateException("HTTP ${conn.responseCode} for $name")
                            }
                            conn.inputStream.use { ins ->
                                val buf = ByteArray(64 * 1024)
                                var offset = start
                                var n = ins.read(buf)
                                while (n > 0) {
                                    synchronized(raf) { raf.seek(offset); raf.write(buf, 0, n) }
                                    offset += n
                                    n = ins.read(buf)
                                }
                            }
                        } finally {
                            conn.disconnect()
                        }
                    }
                } catch (e: Exception) {
                    failures.add("chunk $i: ${e.message}")
                } finally {
                    latch.countDown()
                }
            }
        }
        latch.await()
        raf.close()
        pool.shutdown()

        if (failures.isNotEmpty()) {
            part.delete()
            throw IllegalStateException("download of $name failed: ${failures.joinToString("; ")}")
        }
        if (!part.renameTo(out)) {
            part.copyTo(out, overwrite = true)
            part.delete()
        }
    }
}