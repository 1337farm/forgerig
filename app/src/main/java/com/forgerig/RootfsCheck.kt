package com.forgerig

import java.io.File

/**
 * Single definition of "the guest rootfs is complete enough to boot".
 *
 * Both `bin/sh` AND `usr/bin/sh` must resolve: on Ubuntu `/bin` is a
 * usrmerge symlink to `/usr/bin`, so a partial extraction can leave a bare
 * `bin/sh` behind while the real shell is missing. Booting that guest fails
 * every exec with `proot execve("/usr/bin/sh"): No such file or directory`,
 * which then surfaces as raw proot dumps deep inside installs. Pure
 * `java.io` so unit tests cover it without the Android framework.
 */
object RootfsCheck {
    fun isComplete(filesDir: File): Boolean {
        return try {
            val root = File(filesDir, "ubuntu_rootfs")
            File(root, "bin/sh").exists() && File(root, "usr/bin/sh").exists()
        } catch (e: Exception) {
            false
        }
    }
}
