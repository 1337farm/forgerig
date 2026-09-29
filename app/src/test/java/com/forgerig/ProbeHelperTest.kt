package com.forgerig

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.rules.TemporaryFolder
import org.junit.Test
import java.io.File

class ProbeHelperTest {

    @get:Rule
    val tmp = TemporaryFolder()

    @Test
    fun firstAttemptHasNoDelay() {
        assertEquals(0, ProbeHelper.delayBefore(1))
        assertEquals(0, ProbeHelper.delayBefore(0))
    }

    @Test
    fun delayDoublesThenCaps() {
        assertEquals(1_000, ProbeHelper.delayBefore(2))
        assertEquals(2_000, ProbeHelper.delayBefore(3))
        assertEquals(4_000, ProbeHelper.delayBefore(4))
        assertEquals(8_000, ProbeHelper.delayBefore(5))
        assertEquals(15_000, ProbeHelper.delayBefore(6))
        assertEquals(15_000, ProbeHelper.delayBefore(ProbeHelper.MAX_ATTEMPTS))
    }

    /** The old loop allowed ~3min; the backoff must not cut a slow boot short. */
    @Test
    fun totalBudgetExceedsPreviousFixedIntervalBudget() {
        var total = 0L
        for (a in 1..ProbeHelper.MAX_ATTEMPTS) total += ProbeHelper.delayBefore(a)
        assertTrue("total sleep budget was ${total}ms", total >= 180_000)
    }

    @Test
    fun neverReadStatusIsAlwaysDirty() {
        val dir = tmp.newFolder("status")
        assertTrue(ProbeHelper.statusDirty(File(dir, ".forgerig-status"), -1))
    }

    @Test
    fun unchangedStatusIsNotDirty() {
        val dir = tmp.newFolder("status2")
        val f = File(dir, ".forgerig-status")
        f.writeText("ok")
        val mtime = f.lastModified()
        assertFalse(ProbeHelper.statusDirty(f, mtime))
        f.writeText("exit: 1")
        f.setLastModified(mtime + 5_000)
        assertTrue(ProbeHelper.statusDirty(f, mtime))
    }
}
