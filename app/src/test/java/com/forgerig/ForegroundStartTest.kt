package com.forgerig

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class ForegroundStartTest {

    // Same simple name as the API 31+ framework type, without loading it.
    class ForegroundServiceStartNotAllowedException(message: String) : SecurityException(message)

    @Test
    fun backgroundDenialMatchesFrameworkTypeName() {
        assertTrue(isBackgroundStartDenial(ForegroundServiceStartNotAllowedException("x")))
    }

    @Test
    fun backgroundDenialMatchesAllowStartForegroundMessage() {
        assertTrue(
            isBackgroundStartDenial(
                SecurityException(
                    "Service.startForeground() not allowed due to mAllowStartForeground false"
                )
            )
        )
    }

    @Test
    fun otherSecurityExceptionsStillFailFast() {
        assertFalse(isBackgroundStartDenial(SecurityException("permission denied")))
        assertFalse(isBackgroundStartDenial(SecurityException()))
    }
}
