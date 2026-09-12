package com.forgerig

import android.annotation.SuppressLint
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
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

    override fun onCreate() {
        super.onCreate()
        acquireLocks()
        startForegroundService()
        startContainerProcess()
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        return START_STICKY
    }

    override fun onDestroy() {
        super.onDestroy()
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

    private fun startForegroundService() {
        val channelId = "container_service_channel"
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val channel = NotificationChannel(
                channelId,
                "Container Service",
                NotificationManager.IMPORTANCE_LOW
            )
            val manager = getSystemService(NotificationManager::class.java)
            manager.createNotificationChannel(channel)
        }

        val notification: Notification = NotificationCompat.Builder(this, channelId)
            .setContentTitle("ForgeRig")
            .setContentText("Container daemon is running in the background")
            .setSmallIcon(android.R.drawable.ic_dialog_info)
            .build()

        startForeground(1, notification)
    }

    private fun statusFile(): File = File(filesDir, "ubuntu_rootfs/.forgerig-status")

    private fun writeStatus(text: String) {
        try {
            statusFile().writeText(text)
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
            val notification = NotificationCompat.Builder(this, "container_service_channel")
                .setContentTitle(title)
                .setContentText(message)
                .setSmallIcon(android.R.drawable.ic_dialog_info)
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
            val notification = NotificationCompat.Builder(this, "container_service_channel")
                .setContentTitle("ForgeRig")
                .setContentText(status)
                .setSmallIcon(android.R.drawable.ic_dialog_info)
                .build()
            NotificationManagerCompat.from(this).notify(1, notification)
        } catch (e: Exception) {
            AssetExtractor.logShared(this, "ERROR: updateNotification failed | $e")
        }
    }

    private fun startContainerProcess() {
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
                AssetExtractor.logShared(this, "daemon exited with code $code")
                writeStatus("exit:$code")
                updateNotification("Container stopped (exit $code)")
            } catch (e: Exception) {
                AssetExtractor.logShared(this, "ERROR: container process failed | $e")
                writeStatus("exit:error:${e.message}")
                updateNotification("Container error: ${e.message}")
            }
        }
    }
}
