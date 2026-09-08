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
        val powerManager = getSystemService(Context.POWER_SERVICE) as PowerManager
        wakeLock = powerManager.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "ForgeRig::ContainerWakeLock").apply {
            acquire()
        }

        val wifiManager = applicationContext.getSystemService(Context.WIFI_SERVICE) as WifiManager
        wifiLock = wifiManager.createWifiLock(WifiManager.WIFI_MODE_FULL_HIGH_PERF, "ForgeRig::ContainerWifiLock").apply {
            acquire()
        }
    }

    private fun releaseLocks() {
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

    private fun startContainerProcess() {
        val rootFsDir = File(filesDir, "ubuntu_rootfs")
        val prootBin = File(rootFsDir, "proot")
        val entrypoint = File(rootFsDir, "root/start.sh")

        if (!prootBin.exists() || !entrypoint.exists()) return

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
                pb.environment()["PORT"] = MainActivity.allocatedPort.toString()
                pb.redirectErrorStream(true)
                pb.directory(rootFsDir)

                val process = pb.start()
                containerProcess = process

                process.inputStream.bufferedReader().use { reader ->
                    var line = reader.readLine()
                    while (line != null) {
                        line = reader.readLine()
                    }
                }
                process.waitFor()
            } catch (e: Exception) {
                e.printStackTrace()
            }
        }
    }
}
