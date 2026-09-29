package com.forgerig

import java.io.File

/**
 * Backoff policy for probing the local daemon on launch.
 *
 * The retry is front-loaded (1s, 2s, 4s, 8s, then 15s) so the overwhelmingly
 * common case — daemon answering within a second — still exits on the first
 * or second attempt, while a genuinely slow cold boot is not cut short.
 *
 * The total sleep budget (~240s across 20 attempts) deliberately stays above
 * the previous fixed 1s-per-attempt loop's ~3min worst case: cutting the
 * budget would turn slow devices that currently boot fine into spurious
 * "daemon did not respond" failures, which is a worse outcome than a few
 * redundant probes.
 */
object ProbeHelper {
    const val MAX_ATTEMPTS = 20
    const val BASE_DELAY_MS: Long = 1000
    const val MAX_DELAY_MS: Long = 15_000

    /** Delay to sleep before [attempt] (0 for the first attempt). */
    fun delayBefore(attempt: Int): Long {
        if (attempt <= 1) return 0
        val shift = (attempt - 2).coerceAtMost(20)
        return minOf(BASE_DELAY_MS shl shift, MAX_DELAY_MS)
    }

    /**
     * True when the container status file has changed since [lastMtime]
     * (-1 means "never read", so the first check always reads).
     */
    fun statusDirty(statusFile: File, lastMtime: Long): Boolean =
        lastMtime < 0 || statusFile.lastModified() != lastMtime
}
