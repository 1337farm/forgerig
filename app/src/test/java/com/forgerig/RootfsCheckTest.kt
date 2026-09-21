package com.forgerig

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import java.io.File

class RootfsCheckTest {

    @get:Rule
    val tmp = TemporaryFolder()

    private fun rootWith(vararg paths: String): File {
        val filesDir = tmp.newFolder("files")
        val root = File(filesDir, "ubuntu_rootfs")
        for (p in paths) {
            File(root, p).apply {
                parentFile.mkdirs()
                createNewFile()
            }
        }
        return filesDir
    }

    @Test
    fun missingRootfsIsIncomplete() {
        assertFalse(RootfsCheck.isComplete(tmp.newFolder("empty")))
    }

    @Test
    fun binShAloneIsIncomplete() {
        // Partial extraction: a bare bin/sh without usr/bin/sh boots a
        // broken guest (proot execve("/usr/bin/sh") ENOENT on every exec).
        assertFalse(RootfsCheck.isComplete(rootWith("bin/sh")))
    }

    @Test
    fun usrBinShAloneIsIncomplete() {
        assertFalse(RootfsCheck.isComplete(rootWith("usr/bin/sh")))
    }

    @Test
    fun bothShellsIsComplete() {
        assertTrue(RootfsCheck.isComplete(rootWith("bin/sh", "usr/bin/sh")))
    }
}
