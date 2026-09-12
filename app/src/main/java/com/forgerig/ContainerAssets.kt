package com.forgerig

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
    private const val MAX_CHUNK_ATTEMPTS = 3     // per-chunk retries for transient network/DNS flakes

    data class Asset(val name: String, val sha256: String, val size: Long)

    fun cacheDir(context: Context): File =
        File(context.filesDir, "container").also { it.mkdirs() }

    fun assetFile(context: Context, name: String): File =
        File(cacheDir(context), name)

fun fetchManifest(context: Context): Map<String, Asset> {
        val conn = (URL("$BASE_URL/container-manifest.json").openConnection() as HttpURLConnection).apply {
            connectTimeout = 15_000
            readTimeout = 20_000
        }
        try {
            val text = conn.inputStream.bufferedReader().use { r -> r.readText() }
            writeManifestJsonToCache(context, text)
            return parseManifest(text)
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
     * Write the raw manifest JSON to the app's cache directory so that the daemon
     * can read it without performing its own HTTP fetch (which may fail due to DNS issues).
     */
    private fun writeManifestJsonToCache(context: Context, manifestJson: String) {
        val cacheDir = cacheDir(context)
        val manifestFile = File(cacheDir, "container-manifest.json")
        manifestFile.writeText(manifestJson)
    }

    /**
     * Return a verified asset file: a valid cached copy is used if present,
     * otherwise it is downloaded (chunked + parallel) and sha-verified.
     * [onProgress] reports bytes downloaded so far (called on the worker pool).
     */
    fun ensure(
        context: Context,
        name: String,
        manifest: Map<String, Asset>,
        onProgress: ((Long) -> Unit)? = null,
    ): File {
        val info = manifest[name] ?: throw IllegalStateException("no manifest entry for $name")
        val file = assetFile(context, name)
        if (isValid(file, info)) return file
        download(name, info, file, onProgress)
        if (!isValid(file, info)) {
            // Corrupt assembly (not a resume candidate): drop part + sidecar
            // so the next attempt starts clean instead of looping on bad bytes.
            try { File(file.path + ".part").delete() } catch (e: Exception) { }
            deleteResume(file)
            throw IllegalStateException("download failed sha256 for $name")
        }
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

    // Resume sidecar: "$name|$size|$sha256" on line 1, comma-separated finished
    // chunk indices on line 2. Deliberately hand-rolled (not org.json) so the
    // download path stays usable from plain JVM unit tests.
    private fun readResume(name: String, info: Asset, out: File): MutableSet<Int> {
        val done = mutableSetOf<Int>()
        try {
            val lines = File(out.path + ".resume").readLines()
            if (lines.size < 2) return done
            val head = lines[0].split('|')
            if (head.size != 3 || head[0] != name ||
                head[1] != info.size.toString() || head[2] != info.sha256
            ) return done
            val nChunks = ((info.size + CHUNK - 1) / CHUNK).toInt().coerceAtLeast(1)
            lines[1].split(',').forEach { token ->
                val idx = token.trim().toIntOrNull() ?: return@forEach
                if (idx in 0 until nChunks) done.add(idx)
            }
        } catch (e: Exception) {
        }
        return done
    }

    private fun saveResume(name: String, info: Asset, out: File, done: Set<Int>) {
        try {
            File(out.path + ".resume").writeText(
                "$name|${info.size}|${info.sha256}\n${done.sorted().joinToString(",")}"
            )
        } catch (e: Exception) {
        }
    }

    private fun deleteResume(out: File) {
        try {
            File(out.path + ".resume").delete()
        } catch (e: Exception) {
        }
    }

    internal fun download(
        name: String,
        info: Asset,
        out: File,
        onProgress: ((Long) -> Unit)?,
        connectionFactory: (URL) -> HttpURLConnection = { url -> url.openConnection() as HttpURLConnection },
    ) {
        out.parentFile?.mkdirs()
        val part = File(out.path + ".part")
        val nChunks = ((info.size + CHUNK - 1) / CHUNK).toInt().coerceAtLeast(1)
        Log.i(TAG, "Downloading $name (${info.size} bytes, $nChunks chunks)")
        fun chunkLen(i: Int): Long = min(CHUNK, info.size - i * CHUNK).coerceAtLeast(0L)

        // Resume: keep finished chunks across process death/force-close. The
        // sidecar records them; the final SHA check still guards the assembly,
        // so a corrupt resume can never ship.
        val done = readResume(name, info, out)
        if (!part.exists() || part.length() != info.size) {
            part.delete()
            done.clear()
            deleteResume(out)
            RandomAccessFile(part, "rw").apply { setLength(info.size); close() }
        }
        if (done.isNotEmpty()) {
            Log.i(TAG, "Resuming $name: ${done.size}/$nChunks chunks already present")
        }

        val pool = Executors.newFixedThreadPool(PARALLEL)
        val pending = (0 until nChunks).filter { it !in done }
        val latch = CountDownLatch(pending.size)
        val raf = RandomAccessFile(part, "rw")
        val failures = java.util.Collections.synchronizedList(mutableListOf<String>())
        val written = java.util.concurrent.atomic.AtomicLong(done.sumOf { chunkLen(it) })
        var lastReportedPct = ((written.get() * 100) / info.size).toInt().coerceIn(-1, 100)
        val resumeLock = Any()

        for (i in pending) {
            val start = i * CHUNK
            val end = min(info.size - 1, start + CHUNK - 1)
            pool.execute {
                var attempt = 0
                var completed = false
                var lastError: Exception? = null
                try {
                    while (attempt < MAX_CHUNK_ATTEMPTS && !completed) {
                        attempt++
                        try {
                            if (end >= start) {
                                val conn = connectionFactory(URL("$BASE_URL/$name")).apply {
                                    connectTimeout = 15_000
                                    readTimeout = 60_000
                                    setRequestProperty("Range", "bytes=$start-$end")
                                }
                                try {
                                    val code = conn.responseCode
                                    if (code != 206 && !(code == 200 && start == 0L)) {
                                        throw IllegalStateException("HTTP $code for $name")
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
                                        val chunkBytes = offset - start
                                        if (chunkBytes > 0) {
                                            synchronized(written) {
                                                val nowPct = ((written.addAndGet(chunkBytes) * 100) / info.size).toInt()
                                                if (onProgress != null && nowPct > lastReportedPct) {
                                                    lastReportedPct = nowPct
                                                    onProgress(written.get())
                                                }
                                            }
                                        }
                                    }
                                } finally {
                                    conn.disconnect()
                                }
                            }
                            completed = true
                            synchronized(resumeLock) {
                                done.add(i)
                                saveResume(name, info, out, done)
                            }
                        } catch (e: Exception) {
                            lastError = e
                            if (attempt < MAX_CHUNK_ATTEMPTS) {
                                try {
                                    Thread.sleep(1000L * attempt)
                                } catch (_: InterruptedException) {
                                    Thread.currentThread().interrupt()
                                    break
                                }
                            }
                        }
                    }
                    if (!completed) {
                        failures.add("chunk $i after $attempt attempts: ${lastError?.message}")
                    }
                } finally {
                    latch.countDown()
                }
            }
        }
        latch.await()
        raf.close()
        pool.shutdown()

        if (failures.isNotEmpty()) {
            synchronized(resumeLock) { saveResume(name, info, out, done) }
            throw IllegalStateException(
                "download of $name failed (${done.size}/$nChunks chunks kept — resume on retry): ${failures.joinToString("; ")}"
            )
        }
        if (!part.renameTo(out)) {
            part.copyTo(out, overwrite = true)
            part.delete()
        }
        deleteResume(out)
        onProgress?.invoke(info.size)
    }
}