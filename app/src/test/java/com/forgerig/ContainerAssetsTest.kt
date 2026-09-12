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
}
