package com.forgerig

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import java.io.ByteArrayInputStream
import java.net.HttpURLConnection
import java.net.URL
import java.security.MessageDigest
import java.util.concurrent.atomic.AtomicInteger

class ContainerAssetsTest {
    @get:Rule
    val tmp = TemporaryFolder()

    private class ChunkConnection(url: URL, private val bytes: ByteArray) : HttpURLConnection(url) {
        private var range: String? = null

        override fun connect() {}
        override fun disconnect() {}
        override fun usingProxy(): Boolean = false

        override fun setRequestProperty(key: String, value: String) {
            if (key == "Range") range = value
        }

        override fun getResponseCode(): Int = 206

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
}
