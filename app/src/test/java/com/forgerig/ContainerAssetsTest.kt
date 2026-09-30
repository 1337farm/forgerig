package com.forgerig

import android.content.Context
import android.content.ContextWrapper
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import java.io.ByteArrayInputStream
import java.io.File
import java.net.HttpURLConnection
import java.net.URL
import java.security.MessageDigest
import java.util.concurrent.atomic.AtomicInteger

class ContainerAssetsTest {
    @get:Rule
    val tmp = TemporaryFolder()

    private class ChunkConnection(
        url: URL,
        private val bytes: ByteArray,
        private val seenRanges: MutableList<String>? = null,
    ) : HttpURLConnection(url) {
        private var range: String? = null

        override fun connect() {}
        override fun disconnect() {}
        override fun usingProxy(): Boolean = false

        override fun setRequestProperty(key: String, value: String) {
            if (key == "Range") range = value
        }

        override fun getResponseCode(): Int {
            range?.let { seenRanges?.add(it) }
            return 206
        }

        override fun getInputStream(): ByteArrayInputStream {
            val match = Regex("""bytes=(\d+)-(\d+)""").find(range ?: "")
                ?: throw IllegalStateException("missing Range header")
            val start = match.groupValues[1].toInt()
            val end = match.groupValues[2].toInt()
            return ByteArrayInputStream(bytes.copyOfRange(start, end + 1))
        }
    }

    @Test
    fun downloadRetriesTransientFailureAndAssemblesBytes() {
        val chunk = 4L * 1024 * 1024
        val size = (chunk * 2 + 12345).toInt()
        val expected = ByteArray(size) { (it % 251).toByte() }
        val sha = MessageDigest.getInstance("SHA-256").digest(expected)
            .joinToString("") { "%02x".format(it.toInt() and 0xff) }
        val info = ContainerAssets.Asset("test.bin", sha, size.toLong())
        val calls = AtomicInteger(0)
        val out = tmp.newFile("test.bin")
        ContainerAssets.download("test.bin", info, out, null) { url ->
            if (calls.getAndIncrement() == 0) throw java.net.UnknownHostException("transient dns")
            ChunkConnection(url, expected)
        }
        assertArrayEquals(expected, out.readBytes())
        assertEquals(4, calls.get())
    }

    @Test
    fun downloadResumesCompletedChunksAfterRestart() {
        val chunk = 4L * 1024 * 1024
        val size = (chunk * 2 + 12345).toInt()
        val expected = ByteArray(size) { (it % 251).toByte() }
        val sha = MessageDigest.getInstance("SHA-256").digest(expected)
            .joinToString("") { "%02x".format(it.toInt() and 0xff) }
        val info = ContainerAssets.Asset("resume.bin", sha, size.toLong())
        val out = tmp.newFile("resume.bin")
        // Simulate a previous run that finished chunk 0 then died: part file
        // plus sidecar survive the crash.
        java.io.RandomAccessFile(File(out.path + ".part"), "rw").apply {
            setLength(size.toLong())
            seek(0)
            write(expected, 0, chunk.toInt())
            close()
        }
        File(out.path + ".resume").writeText("resume.bin|$size|$sha\n0")
        val ranges = java.util.Collections.synchronizedList(mutableListOf<String>())
        ContainerAssets.download("resume.bin", info, out, null) { url ->
            ChunkConnection(url, expected, ranges)
        }
        assertArrayEquals(expected, out.readBytes())
        // Chunk 0 was never re-requested; only the 2 missing chunks fetched.
        assertEquals(2, ranges.size)
        assertTrue(ranges.none { it.startsWith("bytes=0-") })
        assertFalse(File(out.path + ".resume").exists())
    }

    @Test
    fun parseManifestReadsVersion2RootfsAndLeanEntries() {
        val text = """
            {
              "version": 2,
              "assets": {
                "ubuntu-rootfs.bin": { "sha256": "abc123", "size": 102579690 },
                "lean-4.35.0-rc3-linux_aarch64.tar.zst": {
                  "sha256": "def456", "size": 596161714,
                  "url": "https://github.com/leanprover/lean4/releases/download/v4.35.0-rc3/lean-4.35.0-rc3-linux_aarch64.tar.zst"
                }
              }
            }
        """.trimIndent()
        val m = ContainerAssets.parseManifest(text)
        assertEquals(2, m.size)
        assertEquals(102579690L, m["ubuntu-rootfs.bin"]!!.size)
        assertEquals("abc123", m["ubuntu-rootfs.bin"]!!.sha256)
        // The Lean entry is carried for the daemon, which reads this same file
        // out of the cache instead of fetching it itself.
        assertEquals(596161714L, m["lean-4.35.0-rc3-linux_aarch64.tar.zst"]!!.size)
    }

    /// ensure() used to hash the cached payload itself even when the caller had
    /// just hashed it to make the same decision, so every install read and
    /// hashed the whole rootfs twice. Passing the verdict through must skip the
    /// second hash — and must never skip the verification that protects the
    /// bytes we actually install.
    @Test
    fun ensureTrustsACallerSuppliedCacheVerdict() {
        val bytes = ByteArray(4096) { (it % 97).toByte() }
        val sha = MessageDigest.getInstance("SHA-256").digest(bytes)
            .joinToString("") { "%02x".format(it.toInt() and 0xff) }
        val info = ContainerAssets.Asset("trusted.bin", sha, bytes.size.toLong())
        val cache = tmp.newFolder("c1")
        val target = File(File(cache, "container"), "trusted.bin").apply {
            parentFile.mkdirs(); writeBytes(bytes)
        }
        val manifest = mapOf("trusted.bin" to info)

        // The caller says "verified": return immediately, no re-hash, and — the
        // important half — do not delete or redownload it. A truthy verdict
        // that silently discarded good bytes would be worse than the slow path.
        assertEquals(
            target,
            ContainerAssets.ensure(ctx(cache), "trusted.bin", manifest, cachedVerified = true),
        )
        assertTrue(target.exists())
        assertArrayEquals(bytes, target.readBytes())
        assertFalse(File(target.path + ".part").exists())
    }

    /// A caller that could not confirm the cache must still get a real check:
    /// `cachedVerified = false` means "I looked and it did not match", so the
    /// download proceeds rather than trusting a stale payload.
    @Test
    fun ensureReDownloadsWhenTheCachedVerdictIsNegative() {
        val bytes = ByteArray(4096) { (it % 97).toByte() }
        val sha = MessageDigest.getInstance("SHA-256").digest(bytes)
            .joinToString("") { "%02x".format(it.toInt() and 0xff) }
        val info = ContainerAssets.Asset("stale.bin", sha, bytes.size.toLong())
        val cache = tmp.newFolder("c2")
        // A truncated payload from a killed download: length already differs.
        File(File(cache, "container"), "stale.bin").apply {
            parentFile.mkdirs(); writeBytes(bytes.copyOf(bytes.size - 10))
        }
        val manifest = mapOf("stale.bin" to info)

        val result = ContainerAssets.ensure(
            ctx(cache),
            "stale.bin",
            manifest,
            cachedVerified = false,
            onProgress = null,
            connectionFactory = { url -> ChunkConnection(url, bytes) },
        )
        assertArrayEquals(bytes, result.readBytes())
    }

    /// Two install threads used to share one `.part`, so whichever renamed first
    /// won and the other could reassemble bytes the first had already written.
    /// The second caller must wait and then observe a finished, verified file.
    @Test
    fun concurrentDownloadsOfTheSameAssetDoNotCorruptEachOther() {
        val chunk = 4L * 1024 * 1024
        val size = (chunk * 2 + 999).toInt()
        val expected = ByteArray(size) { (it % 251).toByte() }
        val sha = MessageDigest.getInstance("SHA-256").digest(expected)
            .joinToString("") { "%02x".format(it.toInt() and 0xff) }
        val info = ContainerAssets.Asset("race.bin", sha, size.toLong())
        val out = tmp.newFile("race.bin")
        val ranges = java.util.Collections.synchronizedList(mutableListOf<String>())
        val start = java.util.concurrent.CountDownLatch(1)
        val errors = java.util.Collections.synchronizedList(mutableListOf<Throwable>())

        val threads = (0 until 4).map {
            Thread {
                try {
                    start.await()
                    ContainerAssets.download("race.bin", info, out, null) { url ->
                        ChunkConnection(url, expected, ranges)
                    }
                } catch (e: Throwable) {
                    errors.add(e)
                }
            }
        }
        threads.forEach { it.start() }
        start.countDown()
        threads.forEach { it.join(60_000) }

        assertTrue("no thread may fail: ${errors.joinToString("; ")}", errors.isEmpty())
        // The bytes on disk must be exactly what the manifest describes, even
        // though four threads raced to produce them.
        assertArrayEquals(expected, out.readBytes())
        // And the ranges were fetched once, not four times over: the losers of
        // the race found a verified file and returned without re-downloading.
        assertTrue(
            "expected 3 chunks fetched once each, got ${ranges.size}",
            ranges.size == 3,
        )
    }

    /// Counts full-payload digests instead of inferring them from a mock
    /// connection, so "the payload is hashed once" is a real assertion.
    ///
    /// This needed a seam. The first version asserted only that `ensure`
    /// returned the right file, which passed with the double-hash still in
    /// place — it cannot distinguish one digest from two.
    @Test
    fun ensureHashesTheCachedPayloadAtMostOnce() {
        val bytes = ByteArray(4096) { (it % 97).toByte() }
        val sha = MessageDigest.getInstance("SHA-256").digest(bytes)
            .joinToString("") { "%02x".format(it.toInt() and 0xff) }
        val info = ContainerAssets.Asset("counted.bin", sha, bytes.size.toLong())
        val cache = tmp.newFolder("c3")
        val target = File(File(cache, "container"), "counted.bin").apply {
            parentFile.mkdirs(); writeBytes(bytes)
        }
        val manifest = mapOf("counted.bin" to info)
        // The digest is instrumented for this test, so it must count reads from
        // zero rather than carrying a count in from setup.
        ContainerAssets.hashReadsForTest.set(0)

        // A positive verdict means the caller already hashed these bytes, so
        // ensure must not digest them again; a null verdict means it must do
        // the check itself. Both are asserted on the count.
        assertEquals("leaked between tests", 0, ContainerAssets.hashReadsForTest.get().toLong())
        ContainerAssets.ensure(ctx(cache), "counted.bin", manifest, cachedVerified = true)
        assertEquals(
            "ensure re-hashed a payload the caller had already verified",
            0L,
            ContainerAssets.hashReadsForTest.get().toLong(),
        )
        // ...and with no verdict it must still do the check itself.
        ContainerAssets.ensure(ctx(cache), "counted.bin", manifest, cachedVerified = null)
        assertEquals(
            "ensure skipped, or doubled, the cache check it was asked to do",
            1L,
            ContainerAssets.hashReadsForTest.get().toLong(),
        )
    }

    /** Minimal Context: these tests only need a filesDir to place the cache. */
    private fun ctx(dir: File): Context =
        object : ContextWrapper(null) {
            override fun getFilesDir(): File = dir
            override fun getAssets() = throw UnsupportedOperationException()
            override fun getApplicationContext(): Context = this
            override fun getPackageName(): String = "com.forgerig"
            override fun getPackageManager() = throw UnsupportedOperationException()
        }

    /// isVerified is what lets a reinstall skip the network: the payload on
    /// disk must match the manifest byte-for-byte, and anything less (short
    /// file, wrong bytes, missing manifest entry) has to re-download.
    @Test
    fun isVerifiedAcceptsOnlyExactManifestBytes() {
        val bytes = ByteArray(2048) { (it % 97).toByte() }
        val sha = MessageDigest.getInstance("SHA-256").digest(bytes)
            .joinToString("") { "%02x".format(it.toInt() and 0xff) }
        val info = ContainerAssets.Asset("rootfs.bin", sha, bytes.size.toLong())
        val good = tmp.newFile("rootfs.bin").apply { writeBytes(bytes) }

        assertTrue(ContainerAssets.isVerified(good, info))
        // Right bytes, wrong manifest: a new release invalidated the cache.
        assertFalse(
            ContainerAssets.isVerified(
                good,
                ContainerAssets.Asset("rootfs.bin", "0".repeat(64), bytes.size.toLong())
            )
        )
        // Truncated by a killed download.
        val partial = tmp.newFile("partial.bin").apply { writeBytes(bytes.copyOf(bytes.size - 1)) }
        assertFalse(ContainerAssets.isVerified(partial, info))
        // Nothing cached, and no manifest entry at all.
        assertFalse(ContainerAssets.isVerified(File(tmp.root, "absent.bin"), info))
        assertFalse(ContainerAssets.isVerified(good, null))
    }
}
