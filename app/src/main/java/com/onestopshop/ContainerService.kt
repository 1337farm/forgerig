package com.onestopshop

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
            wifiLock = wifiManager.createWifiLock(WifiManager.WIFI_MODE_FULL_HIGH_PERF, "ForgeRig::ContainerWifiLock").apply {
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

    private fun notifyMessage(title: String, message: String, id: Int) {
        try {
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

    private fun updateNotification(status: String) {
        try {
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
        val rootFsDir = File(filesDir, "ubuntu_rootfs")
        val prootBin = AssetExtractor.resolveProotFile(this)
        val loaderBin = AssetExtractor.resolveLoaderFile(this)
        val entrypoint = File(rootFsDir, "root/start.sh")

        val missing = mutableListOf<String>()
        if (!prootBin.exists()) missing.add("libproot.so (native lib)")
        if (!loaderBin.exists()) missing.add("libproot_loader.so (native lib)")
        if (!entrypoint.exists()) missing.add("root/start.sh")
        if (missing.isNotEmpty()) {
            val message = "Container files missing: ${missing.joinToString(", ")}. Please run install first."
            AssetExtractor.logShared(this, "ERROR: $message")
            writeStatus("missing:${missing.joinToString(",")}")
            notifyMessage("ForgeRig", message, 2)
            stopSelf()
            return
        }

        AssetExtractor.logShared(this, "Pre-launch: ${AssetExtractor.describeFile(prootBin)}")
        AssetExtractor.logShared(this, "Pre-launch: ${AssetExtractor.describeFile(loaderBin)}")
        val execProblem = AssetExtractor.ensureExecutable(prootBin)
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
                val pb = ProcessBuilder(
                    prootBin.absolutePath,
                    "-r", rootFsDir.absolutePath,
                    "-0",
                    "-w", "/root",
                    "/root/start.sh"
                )
                // The entrypoint launches forgerig-daemon which binds
                // 127.0.0.1:$PORT; the WebView connects to that same port.
                // PROOT_LOADER is mandatory: the Termux-built proot binary
                // hardcodes /data/data/com.termux/... as its loader path,
                // which does not exist on devices without Termux installed.
                pb.environment()["PORT"] = MainActivity.allocatedPort.toString()
                pb.environment()["PROOT_LOADER"] = loaderBin.absolutePath
                pb.redirectErrorStream(true)
                pb.directory(rootFsDir)

                AssetExtractor.logShared(this, "Launching proot (loader=${loaderBin.absolutePath}, port=${MainActivity.allocatedPort})")
                val process = pb.start()
                containerProcess = process
                updateNotification("Container running")

                process.inputStream.bufferedReader().use { reader ->
                    var line = reader.readLine()
                    while (line != null) {
                        AssetExtractor.logShared(this, "proot: $line")
                        line = reader.readLine()
                    }
                }
                val code = process.waitFor()
                AssetExtractor.logShared(this, "proot exited with code $code")
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
