package com.forgerig

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test

class InstallStateTest {

    @Before
    fun reset() {
        InstallState.phase = "idle"
        InstallState.lastProgressAt = 0L
    }

    @Test
    fun tryBeginInstallClaimsOnceThenRejectsSecondClaim() {
        assertTrue(InstallState.tryBeginInstall())
        assertFalse(InstallState.tryBeginInstall())
    }

    @Test
    fun reclaimOnlyFiresAfterIdleWindow() {
        assertTrue(InstallState.tryBeginInstall())
        // Fresh claim (lastProgressAt = now) is not reclaimable yet.
        assertFalse(InstallState.reclaimIfStale(10 * 60 * 1000L))
        // Simulate a dead owner: last progress long past the idle window.
        InstallState.lastProgressAt = System.currentTimeMillis() - 11 * 60 * 1000L
        assertTrue(InstallState.reclaimIfStale(10 * 60 * 1000L))
    }
}