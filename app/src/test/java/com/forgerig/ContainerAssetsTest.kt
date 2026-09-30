package com.forgerig

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
