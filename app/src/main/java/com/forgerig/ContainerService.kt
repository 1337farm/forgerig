package com.forgerig

import android.annotation.SuppressLint
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.net.wifi.WifiManager
import android.os.Build
import android.os.IBinder
import android.os.PowerManager
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import java.io.File
import kotlin.concurrent.thread

class ContainerService : Service() {

    private var wakeLock: PowerManager.WakeLock? = null
    private var wifiLock: WifiManager.WifiLock? = null
    private var containerProcess: Process? = null
    @Volatile
    private var containerStarted = false

    companion object {
        const val ACTION_INSTALL = "com.forgerig.action.INSTALL"
        const val ACTION_START_CONTAINER = "com.forgerig.action.START_CONTAINER"
        const val ACTION_STOP = "com.forgerig.action.STOP"
        /** Broadcast when the service stops so activities finish too (full shutdown). */
        const val ACTION_FINISH_APP = "com.forgerig.action.FINISH_APP"
        private const val NOTIF_ID = 1
        private const val REQ_OPEN = 100
        private const val REQ_STOP = 101
        private const val CHANNEL_ID = "container_service_channel"
        /** An install claim with no progress this long is orphaned: reclaimable. */
        private const val STALE_INSTALL_MS = 10 * 60 * 1000L
    }

    override fun onCreate() {
        super.onCreate()
        AssetExtractor.installCrashHandler(this)
        acquireLocks()
        // Foreground immediately so the notification exists from the moment
        // install is pressed; the container itself starts on ACTION_START_CONTAINER.
        // Must go through startForegroundService(): it creates the notification
        // channel first — startForeground() on an unregistered channel throws
        // Bad notification for startForeground and kills the process.
        startForegroundService()
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_STOP -> shutdown("stop requested")
            ACTION_INSTALL -> startInstall()
            ACTION_START_CONTAINER -> startContainerProcess()
            else -> {
                // Backward compatible plain start (and START_STICKY restart
                // after process death): run the container when the environment
                // is present, resume an interrupted download when partials
                // exist, otherwise stay foreground-idle.
                val root = File(filesDir, "ubuntu_rootfs")
                when {
                    File(root, "bin/sh").exists() -> startContainerProcess()
                    hasPartialDownload() -> {
                        AssetExtractor.logShared(this, "Resuming interrupted install after restart")
                        startInstall()
                    }
                    else -> updateNotification("Preparing…")
                }
            }
        }
        return START_STICKY
    }

    private fun hasPartialDownload(): Boolean {
        return try {
            val dir = File(filesDir, "container")
            dir.listFiles()?.any { f ->
                f.isFile && (f.name.endsWith(".part") || f.name.endsWith(".resume"))
            } == true
        } catch (e: Exception) {
            false
        }
    }

    private var lastInstallNotif = ""

    /** Runs extraction inside the service so it survives the activity going away. */
    private fun startInstall() {
        if (File(filesDir, "ubuntu_rootfs/bin/sh").exists()) {
            startContainerProcess()
            return
        }
        if (!InstallState.tryBeginInstall() && !InstallState.reclaimIfStale(STALE_INSTALL_MS)) {
            AssetExtractor.logShared(this, "Install requested while already installing; ignoring")
            return
        }
        updateNotification("Installing environment…")
        lastInstallNotif = ""
        thread {
            try {
                AssetExtractor(this).setProgressListener(object : InstallProgress {
                    override fun onProgress(percent: Int, stage: String, detail: String) {
                        InstallState.percent = percent
                        InstallState.stage = stage
                        InstallState.detail = detail
                        InstallState.lastProgressAt = System.currentTimeMillis()
                        val text = "Installing… $percent% — $stage"
                        if (text != lastInstallNotif) {
                            lastInstallNotif = text
                            updateNotification(text)
                        }
                    }

                    override fun onStep(step: Int) {
                        InstallState.step = step
                        InstallState.lastProgressAt = System.currentTimeMillis()
                    }

                    override fun onError(message: String, detail: String) {
                        InstallState.phase = "failed"
                        InstallState.error = message
                        InstallState.errorDetail = detail
                        updateNotification("Install failed")
                    }

                    override fun onDone() {
                        // Extraction complete. Do NOT start the container here:
                        // the activity auto-launches on sight of "extracted"
                        // through its existing probe/reload flow.
                        InstallState.phase = "extracted"
                        InstallState.step = 3
                        InstallState.percent = 100
                        InstallState.stage = "Extraction complete — starting container…"
                        updateNotification("Starting container…")
                    }
                }).extractAssets()
            } catch (t: Throwable) {
                InstallState.phase = "failed"
                InstallState.error = "Install crashed"
                InstallState.errorDetail = t.toString()
                AssetExtractor.logShared(this, "ERROR: service install failed | $t\n${t.stackTraceToString()}")
                updateNotification("Install failed")
            }
        }
    }

    override fun onDestroy() {
        super.onDestroy()
        InstallState.cancelled = true
        containerProcess?.destroy()
        releaseLocks()
    }

    override fun onBind(intent: Intent?): IBinder? {
        return null
    }

    private fun acquireLocks() {
        try {
            val powerManager = getSystemService(Context.POWER_SERVICE) as PowerManager
            wakeLock = powerManager.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "ForgeRig::ContainerWakeLock").apply {
                acquire(60 * 60 * 1000L)
            }

            val wifiManager = applicationContext.getSystemService(Context.WIFI_SERVICE) as WifiManager
            // WIFI_MODE_FULL_HIGH_PERF is deprecated since API 33; LOW_LATENCY
            // exists since API 29, so guard by runtime version.
            val wifiMode = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                WifiManager.WIFI_MODE_FULL_LOW_LATENCY
            } else {
                @Suppress("DEPRECATION")
                WifiManager.WIFI_MODE_FULL_HIGH_PERF
            }
            wifiLock = wifiManager.createWifiLock(wifiMode, "ForgeRig::ContainerWifiLock").apply {
                acquire()
            }
        } catch (e: Exception) {
            AssetExtractor.logShared(this, "ERROR: acquireLocks failed | $e")
            releaseLocks()
        }
    }

    private fun releaseLocks() {
        try {
            wakeLock?.let {
                if (it.isHeld) {
                    it.release()
                }
            }
            wifiLock?.let {
                if (it.isHeld) {
                    it.release()
                }
            }
        } catch (e: Exception) {
            AssetExtractor.logShared(this, "ERROR: releaseLocks failed | $e")
        } finally {
            wakeLock = null
            wifiLock = null
        }
    }

    // FLAG_IMMUTABLE only exists on API 31+. Referencing it directly kills
    // the process with NoSuchFieldError on older devices the moment the
    // service starts, so gate it (lint honors the version guard).
    private fun mutableFlag(): Int =
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            PendingIntent.FLAG_IMMUTABLE
        } else {
            0
        }

    private fun openPendingIntent(): PendingIntent {
        val open = Intent(this, MainActivity::class.java).apply {
            action = Intent.ACTION_MAIN
            addCategory(Intent.CATEGORY_LAUNCHER)
            flags = Intent.FLAG_ACTIVITY_SINGLE_TOP or Intent.FLAG_ACTIVITY_CLEAR_TOP
        }
        return PendingIntent.getActivity(
            this, REQ_OPEN, open,
            PendingIntent.FLAG_UPDATE_CURRENT or mutableFlag()
        )
    }

    private fun stopPendingIntent(): PendingIntent {
        val stop = Intent(this, ContainerService::class.java).setAction(ACTION_STOP)
        return PendingIntent.getService(
            this, REQ_STOP, stop,
            PendingIntent.FLAG_UPDATE_CURRENT or mutableFlag()
        )
    }

    private fun buildNotification(text: String): Notification {
        // Tapping opens the app; Open/Stop actions ride on every rebuild so
        // status updates never strip them. Ongoing => not dismissible while
        // the service runs.
        return NotificationCompat.Builder(this, CHANNEL_ID)
            .setContentTitle("ForgeRig")
            .setContentText(text)
            .setSmallIcon(android.R.drawable.ic_dialog_info)
            .setContentIntent(openPendingIntent())
            .addAction(android.R.drawable.ic_menu_view, "Open", openPendingIntent())
            .addAction(android.R.drawable.ic_menu_close_clear_cancel, "Stop", stopPendingIntent())
            .setOngoing(true)
            .setAutoCancel(false)
            .setOnlyAlertOnce(true)
            .setCategory(NotificationCompat.CATEGORY_SERVICE)
            .setVisibility(NotificationCompat.VISIBILITY_PUBLIC)
            .build()
    }

    private fun startForegroundService() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val channel = NotificationChannel(
                CHANNEL_ID,
                "Container Service",
                NotificationManager.IMPORTANCE_LOW
            ).apply { description = "ForgeRig container runtime status" }
            val manager = getSystemService(NotificationManager::class.java)
            manager.createNotificationChannel(channel)
        }
        startForeground(NOTIF_ID, buildNotification("Preparing…"))
    }

    /** Stop the container process and fully shut the service + app down. */
    private fun shutdown(reason: String) {
        // Abort an in-flight install download promptly; the resume sidecar
        // keeps finished chunks for the next attempt.
        InstallState.cancelled = true
        InstallState.phase = "stopped"
        InstallState.stage = "Stopped"
        try {
            containerProcess?.destroy()
        } catch (e: Exception) {
            AssetExtractor.logShared(this, "ERROR: shutdown destroy failed | $e")
        } finally {
            containerProcess = null
            containerStarted = false
        }
        writeStatus("stopped:$reason")
        AssetExtractor.logShared(this, "Container stopped: $reason")
        try {
            sendBroadcast(Intent(ACTION_FINISH_APP).setPackage(packageName))
        } catch (e: Exception) {
            AssetExtractor.logShared(this, "ERROR: finish broadcast failed | $e")
        }
        try {
            stopForeground(Service.STOP_FOREGROUND_REMOVE)
        } catch (e: Exception) {
            AssetExtractor.logShared(this, "ERROR: stopForeground failed | $e")
        }
        stopSelf()
    }

    private fun statusFile(): File = File(filesDir, "ubuntu_rootfs/.forgerig-status")

    private fun writeStatus(text: String) {
        try {
            // The rootfs dir may not exist yet on a fresh install.
            statusFile().apply { parentFile?.mkdirs() }.writeText(text)
        } catch (e: Exception) {
            AssetExtractor.logShared(this, "ERROR: writeStatus failed | $e")
        }
    }

    @SuppressLint("MissingPermission", "NotificationPermission")
    private fun notifyMessage(title: String, message: String, id: Int) {
        try {
            // POST_NOTIFICATIONS is a runtime permission on Android 13+; posting
            // without it is a no-op (and a lint violation). Skip quietly unless
            // notification posting is actually allowed.
            if (!NotificationManagerCompat.from(this).areNotificationsEnabled()) {
                return
            }
            val notification = NotificationCompat.Builder(this, CHANNEL_ID)
                .setContentTitle(title)
                .setContentText(message)
                .setSmallIcon(android.R.drawable.ic_dialog_info)
                .setContentIntent(openPendingIntent())
                .setAutoCancel(true)
                .build()
            NotificationManagerCompat.from(this).notify(id, notification)
        } catch (e: Exception) {
            AssetExtractor.logShared(this, "ERROR: notify failed | $e")
        }
    }

    @SuppressLint("MissingPermission", "NotificationPermission")
    private fun updateNotification(status: String) {
        try {
            if (!NotificationManagerCompat.from(this).areNotificationsEnabled()) {
                return
            }
            NotificationManagerCompat.from(this).notify(NOTIF_ID, buildNotification(status))
        } catch (e: Exception) {
            AssetExtractor.logShared(this, "ERROR: updateNotification failed | $e")
        }
    }

    private fun startContainerProcess() {
        if (containerStarted || containerProcess != null) {
            AssetExtractor.logShared(this, "Container start requested while already running; ignoring")
            return
        }
        containerStarted = true
        // Mark the current run first: MainActivity deletes the stale file
        // before starting us, but the probe may read in between, so claim it
        // here too. Only exit:/missing:/exec-denied: fail the probe.
        writeStatus("starting")
        val rootFsDir = File(filesDir, "ubuntu_rootfs")
        val daemonBin = AssetExtractor.resolveDaemonFile(this)
        val prootBin = AssetExtractor.resolveProotFile(this)
        val loaderBin = AssetExtractor.resolveLoaderFile(this)
        val tallocBin = AssetExtractor.resolveTallocFile(this)

        val missing = mutableListOf<String>()
        if (!daemonBin.exists()) missing.add("libforgerig_daemon.so (native lib)")
        if (!prootBin.exists()) missing.add("libproot.so (native lib)")
        if (!loaderBin.exists()) missing.add("libproot_loader.so (native lib)")
        if (!tallocBin.exists()) missing.add("libtalloc.so.2 (asset dep)")
        if (!rootFsDir.exists()) missing.add("ubuntu_rootfs (extract first)")
        if (missing.isNotEmpty()) {
            val message = "Container files missing: ${missing.joinToString(", ")}. Please run install first."
            AssetExtractor.logShared(this, "ERROR: $message")
            writeStatus("missing:${missing.joinToString(",")}")
            notifyMessage("ForgeRig", message, 2)
            stopSelf()
            return
        }

        AssetExtractor.logShared(this, "Pre-launch daemon: ${AssetExtractor.describeFile(daemonBin)}")
        AssetExtractor.logShared(this, "Pre-launch proot: ${AssetExtractor.describeFile(prootBin)}")
        val execProblem = AssetExtractor.ensureExecutable(daemonBin)
            ?: AssetExtractor.ensureExecutable(prootBin)
            ?: AssetExtractor.ensureExecutable(loaderBin)
        if (execProblem != null) {
            val message = "Container binary is $execProblem"
            AssetExtractor.logShared(this, "ERROR: $message")
            writeStatus("exec-denied:$execProblem")
            notifyMessage("ForgeRig", "Container cannot start: permission denied. See Downloads log.", 2)
            stopSelf()
            return
        }

        writeStatus("running")
        updateNotification("Container starting…")
        thread {
            try {
                // The daemon runs host-side and drives the work guest through
                // proot, so its env carries the container pointers + provider
                // config. It binds 127.0.0.1:$PORT for the WebView.
                val pb = ProcessBuilder(daemonBin.absolutePath)
                pb.environment()["PORT"] = MainActivity.allocatedPort.toString()
                pb.environment()["CONTAINER_PROOT"] = prootBin.absolutePath
                pb.environment()["CONTAINER_ROOTFS"] = rootFsDir.absolutePath
                pb.environment()["PROOT_LOADER"] = loaderBin.absolutePath
                // Payload cache dir: shared with ContainerAssets so the host
                // daemon's on-demand downloads (Lean toolchain) land in the
                // same verified cache the app uses for the rootfs.
                pb.environment()["CONTAINER_CACHE"] = File(filesDir, "container").absolutePath
                // proot is dynamically linked against libtalloc.so.2 +
                // libandroid-shmem.so; the linker finds them via LD_LIBRARY_PATH.
                pb.environment()["LD_LIBRARY_PATH"] = AssetExtractor.loaderSearchPath(this)
                // TLS trust roots for the daemon's OpenSSL-based HTTPS (reqwest
                // via rig): Android has no /etc/ssl/certs, but the system
                // CACerts dir uses OpenSSL hash naming, so point OpenSSL at it.
                pb.environment()["SSL_CERT_DIR"] = "/system/etc/security/cacerts"
                // Guest DNS: generated on the host, the daemon bind-mounts it.
                AssetExtractor.writeResolvConf(this)?.let { resolv ->
                    pb.environment()["CONTAINER_RESOLV_CONF"] = resolv.absolutePath
                }
                // Provider/model/key from the (encrypted) settings store. Only
                // non-empty values are set so the daemon's defaults apply when
                // nothing is configured. Secrets stay in env, never in files.
                SettingsStore.load(this).let { s ->
                    if (s.provider.isNotEmpty()) pb.environment()["FORGERIG_PROVIDER"] = s.provider
                    if (s.model.isNotEmpty()) pb.environment()["FORGERIG_MODEL"] = s.model
                    if (s.evalModel.isNotEmpty()) pb.environment()["FORGERIG_EVAL_MODEL"] = s.evalModel
                    if (s.baseUrl.isNotEmpty()) pb.environment()["FORGERIG_BASE_URL"] = s.baseUrl
                    if (s.apiKey.isNotEmpty()) pb.environment()["FORGERIG_API_KEY"] = s.apiKey
                }
                pb.redirectErrorStream(true)
                pb.directory(File(filesDir, "work").also { it.mkdirs() })

                AssetExtractor.logShared(this, "Launching daemon (port=${MainActivity.allocatedPort}, proot=${prootBin.absolutePath})")
                val process = pb.start()
                containerProcess = process
                updateNotification("Container running")

                process.inputStream.bufferedReader().use { reader ->
                    var line = reader.readLine()
                    while (line != null) {
                        AssetExtractor.logShared(this, "daemon: $line")
                        line = reader.readLine()
                    }
                }
                val code = process.waitFor()
                containerProcess = null
                containerStarted = false
                AssetExtractor.logShared(this, "daemon exited with code $code")
                writeStatus("exit:$code")
                updateNotification("Container stopped (exit $code)")
            } catch (e: Exception) {
                containerProcess = null
                containerStarted = false
                AssetExtractor.logShared(this, "ERROR: container process failed | $e")
                writeStatus("exit:error:${e.message}")
                updateNotification("Container error: ${e.message}")
            }
        }
    }
}
