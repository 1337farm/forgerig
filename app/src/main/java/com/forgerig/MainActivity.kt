package com.forgerig

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.webkit.JavascriptInterface
import android.webkit.WebResourceRequest
import android.webkit.WebResourceResponse
import android.webkit.WebView
import android.webkit.WebViewClient
import androidx.appcompat.app.AppCompatDelegate
import androidx.appcompat.app.AppCompatActivity
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import androidx.core.app.RemoteInput
import org.json.JSONArray
import org.json.JSONObject
import java.io.File
import java.io.OutputStreamWriter
import java.net.HttpURLConnection
import java.net.URL
import java.net.ServerSocket
import java.net.InetAddress
import kotlin.concurrent.thread

class MainActivity : AppCompatActivity() {

    companion object {
        var allocatedPort: Int = 3000

        init {
            try {
                val serverSocket = ServerSocket(0, 1, InetAddress.getByName("127.0.0.1"))
                allocatedPort = serverSocket.localPort
                serverSocket.close()
            } catch (e: Exception) {
                e.printStackTrace()
            }
        }

        const val ACTION_AGENT_REPLY = "com.forgerig.action.AGENT_REPLY"
        const val EXTRA_AGENT_REPLY = "forgerig_reply"
        private const val AGENT_CHANNEL_ID = "agent_updates_channel"
        private const val AGENT_NOTIF_ID = 2
        private const val REQ_AGENT_OPEN = 102
        private const val REQ_AGENT_REPLY = 103
        private const val KEY_TEXT_REPLY = "agent_text_reply"
    }

    /** Manifest-declared receiver so the notification Reply action works even
     * when the activity is gone: it wakes MainActivity with the reply text,
     * which is injected into the WebView composer once the page is ready. */
    class AgentReplyReceiver : BroadcastReceiver() {
        override fun onReceive(context: Context, intent: Intent) {
            if (intent.action != ACTION_AGENT_REPLY) return
            val text = RemoteInput.getResultsFromIntent(intent)?.getCharSequence(KEY_TEXT_REPLY)?.toString()
            try {
                NotificationManagerCompat.from(context).cancel(AGENT_NOTIF_ID)
            } catch (e: Exception) {
            }
            if (text.isNullOrBlank()) return
            try {
                val open = Intent(context, MainActivity::class.java).apply {
                    action = Intent.ACTION_MAIN
                    addCategory(Intent.CATEGORY_LAUNCHER)
                    flags = Intent.FLAG_ACTIVITY_SINGLE_TOP or Intent.FLAG_ACTIVITY_CLEAR_TOP
                    putExtra(EXTRA_AGENT_REPLY, text)
                }
                open.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                context.startActivity(open)
            } catch (e: Exception) {
                AssetExtractor.logShared(context, "ERROR: agent reply open failed | $e")
            }
        }
    }

    private lateinit var webView: WebView
    /** Reply text from the agent-done notification, injected into the
     * composer once the daemon UI (which defines ForgeRigReply) is loaded. */
    @Volatile
    private var pendingAgentReply: String? = null

    // Stop from the notification must shut the whole app, not just the
    // service: the service broadcasts this and every activity finishes.
    private val finishReceiver = object : BroadcastReceiver() {
        override fun onReceive(context: Context?, intent: Intent?) {
            if (intent?.action == ContainerService.ACTION_FINISH_APP) {
                // Full shutdown from the notification: finish this activity AND
                // remove the whole task so nothing (settings back-stack,
                // recents entry) lingers to reopen into.
                finishAffinity()
            }
        }
    }

    private fun registerFinishReceiver() {
        // RECEIVER_NOT_EXPORTED is a compile-time constant (inlined), so this
        // 3-arg call is safe back to minSdk; the flag is ignored pre-33.
        registerReceiver(
            finishReceiver,
            IntentFilter(ContainerService.ACTION_FINISH_APP),
            Context.RECEIVER_NOT_EXPORTED
        )
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        AssetExtractor.installCrashHandler(this)
        AppCompatDelegate.setDefaultNightMode(AppCompatDelegate.MODE_NIGHT_YES)
        super.onCreate(savedInstanceState)
        registerFinishReceiver()
        setContentView(R.layout.activity_main)

        // POST_NOTIFICATIONS is a runtime permission on Android 13+. The
        // foreground-service notification is cosmetic, so request it best-effort
        // (never blocks startup); ContainerService skips notify if denied.
        if (Build.VERSION.SDK_INT >= 33 &&
            checkSelfPermission(android.Manifest.permission.POST_NOTIFICATIONS) != android.content.pm.PackageManager.PERMISSION_GRANTED
        ) {
            try {
                requestPermissions(arrayOf(android.Manifest.permission.POST_NOTIFICATIONS), 1001)
            } catch (_: Exception) {
            }
        }

        // Initialize WebView
        webView = findViewById(R.id.webView)
        webView.setBackgroundColor(0xFF0F1115.toInt())
        webView.settings.apply {
            javaScriptEnabled = true
            domStorageEnabled = true
        }

        // Prevent background sleep cycles, ensuring persistent WebSocket communication
        webView.keepScreenOn = true
        webView.webViewClient = CustomWebViewClient()
        webView.loadUrl("http://127.0.0.1:$allocatedPort")

        // Inject JavascriptInterface to trigger extraction manually
        webView.addJavascriptInterface(WebAppInterface(this), "NativeHost")

        handleIntent(intent)
    }

    inner class CustomWebViewClient : WebViewClient() {
        // Serve bundled art (splash) without touching WebView file-access
        // settings: forgerig.local/* resolves to app assets here.
        override fun shouldInterceptRequest(view: WebView?, request: WebResourceRequest?): WebResourceResponse? {
            try {
                val url = request?.url
                if (url != null && url.host == "forgerig.local") {
                    val name = url.pathSegments.lastOrNull()
                    if (name == "splash.jpeg") {
                        val stream = view?.context?.assets?.open("splash.jpeg")
                        if (stream != null) {
                            return WebResourceResponse("image/jpeg", null, stream)
                        }
                    }
                }
            } catch (e: Exception) {
            }
            return super.shouldInterceptRequest(view, request)
        }

        override fun shouldOverrideUrlLoading(view: WebView?, request: android.webkit.WebResourceRequest?): Boolean {
            val url = request?.url
            if (url != null && url.scheme == "forgerig") {
                val intent = Intent(Intent.ACTION_VIEW, url)
                startActivity(intent)
                return true
            }
            return super.shouldOverrideUrlLoading(view, request)
        }

        override fun onReceivedError(
            view: WebView?,
            request: android.webkit.WebResourceRequest?,
            error: android.webkit.WebResourceError?
        ) {
            if (request?.isForMainFrame == true) {
                val fallbackHtml = """
                    <html>
                    <head>
                        <meta name="viewport" content="width=device-width, initial-scale=1">
                        <meta name="color-scheme" content="dark">
                        <style>
                            body { font-family: sans-serif; display: flex; justify-content: center; align-items: center; height: 100vh; margin: 0; background-color: #0f1115; color: #e6e6e6; }
                            h2 { color: #e6e6e6; }
                            .message { text-align: center; padding: 20px; background: #1b1f27; border: 1px solid #333; border-radius: 8px; box-shadow: 0 4px 6px rgba(0,0,0,0.35); width: 84%; max-width: 420px; }
                            .btn { background-color: #3498db; border: 1px solid #2980b9; color: white; padding: 15px 32px; text-align: center; text-decoration: none; display: inline-block; font-size: 16px; margin: 4px 2px; cursor: pointer; border-radius: 8px; }
                            .btn:disabled { background-color: #33505f; border-color: #33505f; cursor: default; }
                            .progress-track { margin: 12px 0 8px; height: 12px; background-color: #2a2f3a; border: 1px solid #333; border-radius: 6px; overflow: hidden; display: none; }
                            .progress-fill { height: 100%; width: 0; background-color: #3498db; border-radius: 6px; transition: width 0.2s ease; }
                            .step-count { margin: 8px 0 4px; font-size: 14px; font-weight: bold; color: #e6e6e6; display: none; }
                            .steps { list-style: none; margin: 8px 0; padding: 0; text-align: left; display: none; }
                            .steps li { margin: 6px 0; font-size: 14px; color: #8a8f9a; }
                            .steps li.done { color: #7cf787; text-decoration: line-through; }
                            .steps li.active { color: #e6e6e6; font-weight: bold; }
                            .steps li.failed { color: #ff7b72; font-weight: bold; }
                            .steps .dot { display: inline-block; width: 10px; height: 10px; margin-right: 6px; border-radius: 50%; background-color: #3498db; animation: pulse 1s infinite; }
                            @keyframes pulse { 50% { opacity: 0.25; } }
                            .detail { margin: 4px 0 8px; font-size: 12px; color: #b8bcc4; display: none; word-break: break-all; text-align: left; background: #141820; border: 1px solid #333; padding: 6px; border-radius: 4px; max-height: 100px; overflow-y: auto; }
                            .error { margin: 12px 0; padding: 10px; border-radius: 6px; background-color: #381d1d; border: 1px solid #7a2b2b; color: #ffb4ab; font-size: 14px; display: none; word-break: break-all; text-align: left; }
                            #splash { display: none; text-align: center; padding: 16px 6px 10px; }
                            #splash .logo { font-size: 26px; font-weight: bold; color: #e6e6e6; letter-spacing: 1px; }
                            #splash .sub { margin: 12px 0 4px; font-size: 13px; color: #8a8f9a; }
                            #splash .spin { width: 34px; height: 34px; margin: 14px auto 0; border-radius: 50%; border: 3px solid #2a2f3a; border-top-color: #3498db; animation: spin 0.9s linear infinite; }
                            #splash img { width: 100%; max-width: 380px; border-radius: 12px; border: 1px solid #333; }
                            @keyframes spin { to { transform: rotate(360deg); } }
                        </style>
                        <script>
                            var installState = null;

                            var reloading = false;
                            // True once THIS page pressed Install: keeps the fresh-install
                            // steps UI even after bin/sh lands mid-extraction (which would
                            // otherwise flip isRelaunch() to the splash).
                            var freshInstall = false;
                            var FALLBACK_LABELS = ['Prepare runtime', 'Unpack container files', 'Finalize environment', 'Start container service', 'Connect to daemon'];

                            function stepLabels() {
                                if (installState && installState.stepLabels && installState.stepLabels.length) {
                                    return installState.stepLabels;
                                }
                                return FALLBACK_LABELS;
                            }

                            function stepsTotal() {
                                return (installState && installState.stepsTotal) || FALLBACK_LABELS.length;
                            }

                            function pollInstall() {
                                if (window.NativeHost) {
                                    var raw = window.NativeHost.getInstallState();
                                    if (raw) {
                                        try { installState = JSON.parse(raw); } catch (e) {}
                                    }
                                    // Re-attach to whatever the service is doing: an
                                    // installed env (or a just-finished extraction)
                                    // auto-launches the container; an idle state with
                                    // partial download files resumes the install.
                                    // Re-entry guards live on the native side
                                    // (shared state), so repeated ticks are no-ops.
if (installState && typeof window.NativeHost.isInstalled === 'function') {
                                    var inst = window.NativeHost.isInstalled();
                                    if (inst && (installState.phase === 'idle' ||
                                        installState.phase === 'extracted' ||
                                        installState.phase === 'stopped')) {
                                        // Installed env whose driver isn't running (fresh
                                        // open, just-finished extraction, or a prior Stop
                                        // left phase "stopped") — relaunch, don't show
                                        // the install screen.
                                        window.NativeHost.launchExisting();
                                    } else if (installState.phase === 'idle' &&
                                        typeof window.NativeHost.hasPartialDownload === 'function' &&
                                        window.NativeHost.hasPartialDownload()) {
                                        window.NativeHost.installNow();
                                    }
                                }
                                }
                                renderInstall();
                                setTimeout(pollInstall, 300);
                            }

                            function renderSteps(failedStep) {
                                var labels = stepLabels();
                                var total = stepsTotal();
                                var cur = installState.step;
                                var html = '';
                                var doneCount = 0;
                                for (var i = 0; i < total; i++) {
                                    var label = labels[i] || ('Step ' + (i + 1));
                                    var cls, mark;
                                    if (installState.phase === 'ready' || i < cur) {
                                        cls = 'done'; mark = '&#10003; '; doneCount++;
                                    } else if (failedStep && i === cur) {
                                        cls = 'failed'; mark = '&#10007; ';
                                    } else if (i === cur) {
                                        cls = 'active'; mark = '<span class="dot"></span>';
                                    } else {
                                        cls = 'pending'; mark = '';
                                    }
                                    html += '<li class="' + cls + '">' + mark + (i + 1) + '. ' + label + '</li>';
                                }
                                document.getElementById('steps').innerHTML = html;
                                return doneCount;
                            }

                            function isRelaunch() {
                                try {
                                    return !freshInstall && installState && installState.phase === 'installing' &&
                                        window.NativeHost && window.NativeHost.isInstalled &&
                                        window.NativeHost.isInstalled();
                                } catch (e) { return false; }
                            }

                            function showSplash(text) {
                                ['title', 'step-count', 'steps', 'progress-track', 'detail-text', 'error-text', 'log-path', 'install-btn', 'retry-btn', 'copy-btn', 'settings-btn'].forEach(function(id) {
                                    var el = document.getElementById(id);
                                    if (el) el.style.display = 'none';
                                });
                                document.getElementById('splash-text').innerText = text || 'Starting…';
                                document.getElementById('splash').style.display = 'block';
                            }

                            function renderInstall() {
                                if (!installState) { return; }

                                var phase = installState.phase;
                                var bar = document.getElementById('progress-fill');
                                var track = document.getElementById('progress-track');
                                var count = document.getElementById('step-count');
                                var steps = document.getElementById('steps');
                                var detail = document.getElementById('detail-text');
                                var err = document.getElementById('error-text');
                                var installBtn = document.getElementById('install-btn');
                                var retryBtn = document.getElementById('retry-btn');
                                var copyBtn = document.getElementById('copy-btn');

                                if (phase === 'installing' && isRelaunch()) {
                                    // Reopened into an installed env (or mid-launch):
                                    // splash instead of the install steps.
                                    showSplash(installState.stage || 'Starting ForgeRig…');
                                    return;
                                }

                                if (phase === 'installing') {
                                    installBtn.style.display = 'none';
                                    retryBtn.style.display = 'none';
                                    copyBtn.style.display = 'none';
                                    err.style.display = 'none';
                                    var total = stepsTotal();
                                    var cur = Math.max(0, installState.step);
                                    var doneCount = renderSteps(false);
                                    steps.style.display = 'block';
                                    count.style.display = 'block';
                                    count.innerText = 'Step ' + Math.min(cur + 1, total) + ' of ' + total + ' — ' + (stepLabels()[cur] || '');
                                    var extracting = cur <= 2;
                                    track.style.display = extracting ? 'block' : 'none';
                                    if (extracting) { bar.style.width = installState.percent + '%'; }
                                    detail.style.display = installState.detail ? 'block' : 'none';
                                    detail.innerText = installState.detail;
                                } else if (phase === 'ready') {
                                    showSplash('Environment ready — opening workspace…');
                                    var reloads = 0;
                                    try { reloads = parseInt(sessionStorage.getItem('readyReloads') || '0', 10); } catch (e) {}
                                    if (reloads < 3 && !reloading) {
                                        reloading = true;
                                        try { sessionStorage.setItem('readyReloads', String(reloads + 1)); } catch (e) {}
                                        setTimeout(function() { window.location.reload(); }, 1500);
                                    } else if (reloads >= 3) {
                                        retryBtn.style.display = 'inline-block';
                                    }
                                } else if (phase === 'failed') {
                                    installBtn.style.display = 'inline-block';
                                    installBtn.disabled = false;
                                    installBtn.innerText = 'Retry Install';
                                    copyBtn.style.display = 'inline-block';
                                    var logPath = document.getElementById('log-path');
                                    logPath.style.display = 'block';
                                    logPath.innerText = 'Full log: Downloads/' + (installState.logFile || 'forgerig-install-*.log');
                                    track.style.display = 'none';
                                    renderSteps(true);
                                    steps.style.display = 'block';
                                    count.style.display = 'block';
                                    count.innerText = 'Failed at step ' + (installState.step + 1) + ' of ' + stepsTotal();
                                    detail.style.display = installState.detail ? 'block' : 'none';
                                    detail.innerText = installState.detail || '';
                                    err.style.display = 'block';
                                    err.innerText = installState.errorDetail ?
                                        installState.error + '\n\n' + installState.errorDetail :
                                        installState.error;
                                } else {
                                    installBtn.style.display = 'inline-block';
                                    installBtn.disabled = false;
                                    installBtn.innerText = 'Install Environment';
                                    retryBtn.style.display = 'none';
                                    copyBtn.style.display = 'none';
                                    track.style.display = 'none';
                                    count.style.display = 'none';
                                    steps.style.display = 'none';
                                    detail.style.display = 'none';
                                    err.style.display = 'none';
                                    document.getElementById('log-path').style.display = 'none';
                                }
                            }

                            function copyLogs() {
                                var text = "";
                                if (installState) {
                                    text = "Phase: " + installState.phase + "\nStep: " + installState.step + "/" + installState.stepsTotal + "\nStage: " + installState.stage + "\nDetail: " + installState.detail + "\nError: " + installState.error + "\nErrorDetail: " + installState.errorDetail + "\nLogFile: " + installState.logFile;
                                } else {
                                    text = document.getElementById('error-text').innerText || document.getElementById('step-count').innerText;
                                }
                                function legacyCopy(t) {
                                    var ta = document.createElement('textarea');
                                    ta.value = t;
                                    ta.style.position = 'fixed';
                                    ta.style.opacity = '0';
                                    document.body.appendChild(ta);
                                    ta.select();
                                    try {
                                        document.execCommand('copy');
                                        alert("Logs copied to clipboard!");
                                    } catch (e) {
                                        alert("Copy failed. Log file: " + (installState && installState.logFile ? installState.logFile : "Downloads/forgerig-install-*.log"));
                                    }
                                    document.body.removeChild(ta);
                                }
                                if (navigator.clipboard && navigator.clipboard.writeText) {
                                    navigator.clipboard.writeText(text).then(function() {
                                        alert("Logs copied to clipboard!");
                                    }, function(err) {
                                        legacyCopy(text);
                                    });
                                } else {
                                    legacyCopy(text);
                                }
                            }

                            function install() {
                                if (window.NativeHost) {
                                    try { sessionStorage.setItem('readyReloads', '0'); } catch (e) {}
                                    freshInstall = true;
                                    installState = {phase: 'installing', step: 0, stepsTotal: 5, stepLabels: stepLabels(), percent: 0, stage: 'Starting…', detail: '', error: '', errorDetail: '', logFile: (installState && installState.logFile) || 'forgerig-install.log'};
                                    renderInstall();
                                    window.NativeHost.installNow();
                                }
                            }

                            function openSettings() {
                                if (window.NativeHost) { try { window.NativeHost.openSettings(); } catch (e) {} }
                            }

                            window.onload = function() {
                                pollInstall();
                            };
                        </script>
                    </head>
                    <body>
                        <div class="message">
                            <h2 id="title">ForgeRig</h2>
                            <div id="splash"><img src="https://forgerig.local/splash.jpeg" alt="ForgeRig"/><div class="spin"></div><p class="sub" id="splash-text">Starting…</p></div>
                            <p class="step-count" id="step-count"></p>
                            <ol class="steps" id="steps"></ol>
                            <div class="progress-track" id="progress-track"><div class="progress-fill" id="progress-fill"></div></div>
                            <p class="detail" id="detail-text"></p>
                            <div class="error" id="error-text"></div>
                            <p class="detail" id="log-path" style="display:none;"></p>
                            <button id="install-btn" class="btn" style="display:none;" onclick="install()">Install Environment</button>
                            <br><button id="retry-btn" class="btn" style="display:none;" onclick="window.location.reload()">Retry Connection</button>
                            <br><button id="copy-btn" class="btn" style="background-color: #7f8c8d; margin-top: 8px; font-size: 14px; padding: 10px 20px; display: none;" onclick="copyLogs()">Copy Logs</button>
                            <br><button id="settings-btn" class="btn" style="background-color: #555; margin-top: 8px; font-size: 14px; padding: 10px 20px;" onclick="openSettings()">⚙ Settings</button>
                        </div>
                    </body>
                    </html>
                """.trimIndent()
                view?.loadDataWithBaseURL(request.url.toString(), fallbackHtml, "text/html", "UTF-8", null)
            } else {
                super.onReceivedError(view, request, error)
            }
        }

        override fun onPageFinished(view: WebView?, url: String?) {
            super.onPageFinished(view, url)
            // Daemon UI (or a reload) is ready: deliver any pending
            // notification reply into the composer.
            injectAgentReply()
        }
    }

    inner class WebAppInterface(private val context: MainActivity) {

        @Volatile
        private var phase: String = "idle"
        @Volatile
        private var percent: Int = 0
        @Volatile
        private var stage: String = ""
        @Volatile
        private var detail: String = ""
        @Volatile
        private var error: String = ""
        @Volatile
        private var errorDetail: String = ""
        @Volatile
        private var step: Int = -1

        private val stepLabels = listOf(
            "Prepare runtime",
            "Unpack container files",
            "Finalize environment",
            "Start container service",
            "Connect to daemon"
        )

        @JavascriptInterface
        fun getInstallState(): String {
            // Mirror the service-owned state first: a reopened activity
            // re-attaches to an install already running in the service.
            phase = InstallState.phase
            percent = InstallState.percent
            stage = InstallState.stage
            detail = InstallState.detail
            error = InstallState.error
            errorDetail = InstallState.errorDetail
            step = InstallState.step
            return JSONObject()
                .put("phase", phase)
                .put("percent", percent)
                .put("stage", stage)
                .put("detail", detail)
                .put("error", error)
                .put("errorDetail", errorDetail)
                .put("logFile", AssetExtractor.sharedLogFileName())
                .put("step", step)
                .put("stepsTotal", stepLabels.size)
                .put("stepLabels", JSONArray(stepLabels))
                .toString()
        }

        @JavascriptInterface
        fun openSettings() {
            try {
                context.startActivity(Intent(context, SettingsActivity::class.java))
            } catch (e: Exception) {
                AssetExtractor.logShared(context, "ERROR: openSettings failed | $e")
            }
        }

        /** Mirror the agent's working state into the foreground-service
         * notification so the drawer shows live status/progress. */
        @JavascriptInterface
        fun reportAgentStatus(text: String) {
            try {
                val status = Intent(context, ContainerService::class.java)
                    .setAction(ContainerService.ACTION_AGENT_STATUS)
                    .putExtra(ContainerService.EXTRA_AGENT_STATUS, text)
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                    context.startForegroundService(status)
                } else {
                    context.startService(status)
                }
            } catch (t: Throwable) {
                AssetExtractor.logShared(context, "ERROR: reportAgentStatus failed | $t")
            }
        }

        /** Agent finished a turn: ping the drawer (only when the app is
         * backgrounded — the caller checks visibility) with the reply
         * preview, an Open action, and an inline Reply action that feeds
         * straight back into the composer. */
        @JavascriptInterface
        fun notifyAgentDone(preview: String) {
            try {
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                    val channel = NotificationChannel(
                        AGENT_CHANNEL_ID,
                        "Agent updates",
                        NotificationManager.IMPORTANCE_DEFAULT
                    ).apply { description = "Agent reply status and quick reply" }
                    val manager = context.getSystemService(NotificationManager::class.java)
                    manager.createNotificationChannel(channel)
                }
                val open = Intent(context, MainActivity::class.java).apply {
                    action = Intent.ACTION_MAIN
                    addCategory(Intent.CATEGORY_LAUNCHER)
                    flags = Intent.FLAG_ACTIVITY_SINGLE_TOP or Intent.FLAG_ACTIVITY_CLEAR_TOP
                }
                val openPi = PendingIntent.getActivity(
                    context, REQ_AGENT_OPEN, open,
                    PendingIntent.FLAG_UPDATE_CURRENT or immutableFlag()
                )
                // RemoteInput needs a MUTABLE PendingIntent on API 31+.
                val mutable = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
                    PendingIntent.FLAG_MUTABLE
                } else {
                    0
                }
                val replyIntent = Intent(context, AgentReplyReceiver::class.java)
                    .setAction(ACTION_AGENT_REPLY)
                val replyPi = PendingIntent.getBroadcast(
                    context, REQ_AGENT_REPLY, replyIntent,
                    PendingIntent.FLAG_UPDATE_CURRENT or mutable
                )
                val remoteInput = RemoteInput.Builder(KEY_TEXT_REPLY)
                    .setLabel("Reply to agent…")
                    .build()
                val replyAction = NotificationCompat.Action.Builder(
                    android.R.drawable.ic_menu_send, "Reply", replyPi
                ).addRemoteInput(remoteInput).build()
                val body = preview.ifBlank { "The agent finished and is ready for more." }
                val notif = NotificationCompat.Builder(context, AGENT_CHANNEL_ID)
                    .setContentTitle("ForgeRig agent ready")
                    .setContentText(body)
                    .setStyle(NotificationCompat.BigTextStyle().bigText(body))
                    .setSmallIcon(android.R.drawable.ic_dialog_info)
                    .setContentIntent(openPi)
                    .addAction(replyAction)
                    .addAction(android.R.drawable.ic_menu_view, "Open", openPi)
                    .setAutoCancel(true)
                    .setOnlyAlertOnce(false)
                    .setCategory(NotificationCompat.CATEGORY_MESSAGE)
                    .setVisibility(NotificationCompat.VISIBILITY_PRIVATE)
                    .build()
                NotificationManagerCompat.from(context).notify(AGENT_NOTIF_ID, notif)
            } catch (t: Throwable) {
                AssetExtractor.logShared(context, "ERROR: notifyAgentDone failed | $t")
            }
        }

        private fun immutableFlag(): Int =
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
                PendingIntent.FLAG_IMMUTABLE
            } else {
                0
            }

        @JavascriptInterface
        fun installNow() {
            // The service is the SINGLE owner of the install claim: its
            // startInstall() atomically claims via tryBeginInstall() and starts
            // the one extraction thread. Claiming here first seizes the claim,
            // so the service's startInstall() then sees "already installing"
            // (reclaimIfStale only fires after 10 idle minutes) and never
            // starts — the stuck "already installing" on every fresh install.
            // Our fields are only a UI mirror; the service dedupes repeat
            // ACTION_INSTALLs through the same claim.
            phase = "installing"
            percent = 0
            step = 0
            stage = "Preparing…"
            detail = ""
            error = ""
            errorDetail = ""
            // Extraction runs in the foreground service (survives swipe-away);
            // progress is polled back through getInstallState().
            startServiceAction(ContainerService.ACTION_INSTALL)
        }

        /** True once the Ubuntu rootfs has been extracted (environment installed). */
        @JavascriptInterface
        fun isInstalled(): Boolean {
            return try {
                val sh = File(context.filesDir, "ubuntu_rootfs/bin/sh")
                sh.exists()
            } catch (e: Exception) {
                false
            }
        }

        /**
         * Reopen path: the environment is already installed (or just finished
         * extracting in the service), so skip extraction and just (re)start
         * the container service + wait for the daemon. Reuses the same
         * `phase == "installing"` probe loop so its status/ready/failed handling applies.
         */
        @JavascriptInterface
        fun launchExisting() {
            // Single-owner: a tap racing this method must not double-launch.
            if (!InstallState.tryBeginInstall()) {
                AssetExtractor.logShared(context, "launchExisting ignored: claim already held")
                return
            }
            phase = "installing"
            InstallState.step = 3
            step = 3
            InstallState.stage = "Starting container service…"
            stage = "Starting container service…"
            InstallState.detail = ""
            detail = ""
            InstallState.error = ""
            error = ""
            InstallState.errorDetail = ""
            errorDetail = ""
            launchContainerAndWait()
        }

        /** True when a previous run left partial download files to resume. */
        @JavascriptInterface
        fun hasPartialDownload(): Boolean {
            return try {
                val dir = File(context.filesDir, "container")
                dir.listFiles()?.any { f ->
                    f.isFile && (f.name.endsWith(".part") || f.name.endsWith(".resume"))
                } == true
            } catch (e: Exception) {
                false
            }
        }

        private fun startServiceAction(action: String) {
            try {
                val serviceIntent = Intent(context, ContainerService::class.java).setAction(action)
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                    context.startForegroundService(serviceIntent)
                } else {
                    context.startService(serviceIntent)
                }
            } catch (t: Throwable) {
                // Throwable, not Exception: an Error here (e.g. a missing
                // framework symbol on old devices) must land in the shared
                // log, never kill the app from the JS bridge thread.
                AssetExtractor.logShared(context, "ERROR: service action $action failed | $t")
            }
        }

        private fun startContainerInternal() {
            startServiceAction(ContainerService.ACTION_START_CONTAINER)
        }

        private fun stopContainerService() {
            startServiceAction(ContainerService.ACTION_STOP)
        }

        /** App-side stop hook (used by Settings): kills the container + service. */
        @JavascriptInterface
        fun stopContainer() {
            stopContainerService()
        }

        private fun launchContainerAndWait() {
            step = 3
            stage = "Starting container service…"
            detail = ""
            thread {
                AssetExtractor.logShared(context, "Starting container service (build=${AssetExtractor.buildId()})")
                try {
                    // Drop the previous run's verdict first: the probe fails fast
                    // on exit:/missing:/exec-denied:, and without this reset it
                    // would read the stale file before the service overwrites it.
                    val stale = File(context.filesDir, "ubuntu_rootfs/.forgerig-status")
                    if (stale.exists() && !stale.delete()) {
                        AssetExtractor.logShared(context, "WARNING: could not delete stale status file")
                    }
                    startContainerInternal()
                } catch (e: Exception) {
                    AssetExtractor.logShared(context, "ERROR: Could not start container service | $e")
                    phase = "failed"
                    error = "Could not start container service"
                    errorDetail = e.toString()
                    InstallState.phase = "failed"
                    InstallState.error = error
                    InstallState.errorDetail = errorDetail
                    stopContainerService()
                    return@thread
                }
                step = 4
                stage = "Connecting to daemon…"
                val maxAttempts = 60
                var attempt = 0
                var ready = false
                val statusFile = File(context.filesDir, "ubuntu_rootfs/.forgerig-status")
                while (attempt < maxAttempts && phase == "installing") {
                    try {
                        if (statusFile.exists()) {
                            val status = statusFile.readText().trim()
                            if (status.startsWith("exit:") || status.startsWith("missing:") || status.startsWith("exec-denied:")) {
                                AssetExtractor.logShared(context, "ERROR: Container status: $status")
                                phase = "failed"
                                error = "Container exited early"
                                errorDetail = "Container status: $status. See Downloads/${AssetExtractor.sharedLogFileName()} for details."
                                InstallState.phase = "failed"
                                InstallState.error = error
                                InstallState.errorDetail = errorDetail
                                break
                            }
                        }
                    } catch (e: Exception) {
                        // Status unreadable; keep probing HTTP.
                    }
                    attempt++
                    detail = "Probing daemon on 127.0.0.1:${MainActivity.allocatedPort} (attempt $attempt/$maxAttempts)…"
                    try {
                        val url = URL("http://127.0.0.1:${MainActivity.allocatedPort}/")
                        val conn = url.openConnection() as HttpURLConnection
                        conn.connectTimeout = 1500
                        conn.readTimeout = 1500
                        conn.requestMethod = "GET"
                        conn.connect()
                        val code = conn.responseCode
                        conn.disconnect()
                        if (code in 200..499) {
                            AssetExtractor.logShared(context, "Daemon responded with HTTP $code on attempt $attempt")
                            ready = true
                            break
                        }
                        AssetExtractor.logShared(context, "Probe attempt $attempt/$maxAttempts: HTTP $code")
                    } catch (e: Exception) {
                        AssetExtractor.logShared(context, "Probe attempt $attempt/$maxAttempts failed: ${e.message}")
                    }
                    Thread.sleep(1000)
                }
                if (ready) {
                    AssetExtractor.logShared(context, "DONE: Container ready, opening workspace")
                    percent = 100
                    phase = "ready"
                    InstallState.percent = 100
                    InstallState.phase = "ready"
                } else if (phase == "installing") {
                    AssetExtractor.logShared(context, "ERROR: Container did not respond after $maxAttempts attempts")
                    phase = "failed"
                    error = "Container did not respond in time"
                    errorDetail = "Timed out after $maxAttempts attempts probing " +
                        "http://127.0.0.1:${MainActivity.allocatedPort}/. The service started but " +
                        "nothing serves HTTP. See Downloads/${AssetExtractor.sharedLogFileName()} for details."
                    InstallState.phase = "failed"
                    InstallState.error = error
                    InstallState.errorDetail = errorDetail
                    stopContainerService()
                }
            }
        }
    }

    override fun onDestroy() {
        try {
            unregisterReceiver(finishReceiver)
        } catch (e: Exception) {
        }
        super.onDestroy()
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        handleIntent(intent)
    }

    /** Feed a notification reply into the chat composer. Probes for the
     * ForgeRigReply bridge first so a fallback/install page (which lacks it)
     * keeps the text pending instead of dropping it. */
    private fun injectAgentReply() {
        val text = pendingAgentReply ?: return
        if (!::webView.isInitialized) return
        try {
            webView.post {
                try {
                    webView.evaluateJavascript("typeof ForgeRigReply") { type ->
                        if (type?.trim('"') == "function") {
                            pendingAgentReply = null
                            webView.evaluateJavascript(
                                "ForgeRigReply(${org.json.JSONObject.quote(text)})",
                                null
                            )
                        }
                    }
                } catch (e: Exception) {
                    AssetExtractor.logShared(this, "ERROR: agent reply inject failed | $e")
                }
            }
        } catch (e: Exception) {
            AssetExtractor.logShared(this, "ERROR: agent reply post failed | $e")
        }
    }

    private fun handleIntent(intent: Intent?) {
        val action = intent?.action
        val data: Uri? = intent?.data

        // Reply typed in the agent-done notification: hold it for injection
        // once the page finishes loading, and try immediately in case the
        // WebView is already on the daemon UI.
        intent?.getStringExtra(EXTRA_AGENT_REPLY)?.takeIf { it.isNotBlank() }?.let {
            pendingAgentReply = it
            injectAgentReply()
        }

        if (Intent.ACTION_VIEW == action && data != null) {
            if (data.scheme == "forgerig" && data.host == "oauth-callback") {
                val code = data.getQueryParameter("code")
                if (code != null) {
                    exchangeCodeForToken(code)
                }
            }
        }
    }

    private fun exchangeCodeForToken(code: String) {
        thread {
            try {
                val url = URL("https://github.com/login/oauth/access_token")
                val connection = url.openConnection() as HttpURLConnection
                connection.requestMethod = "POST"
                connection.setRequestProperty("Accept", "application/json")
                connection.doOutput = true

                val clientId = BuildConfig.GITHUB_CLIENT_ID
                val clientSecret = BuildConfig.GITHUB_CLIENT_SECRET
                val postData = "client_id=$clientId&client_secret=$clientSecret&code=$code"

                OutputStreamWriter(connection.outputStream).use { writer ->
                    writer.write(postData)
                    writer.flush()
                }

                if (connection.responseCode == HttpURLConnection.HTTP_OK) {
                    val response = connection.inputStream.bufferedReader().use { it.readText() }
                    val jsonObject = JSONObject(response)
                    if (jsonObject.has("access_token")) {
                        val accessToken = jsonObject.getString("access_token")
                        writeTokenToGitConfig(accessToken)
                    }
                }
            } catch (e: Exception) {
                AssetExtractor.logShared(this@MainActivity, "ERROR: GitHub token exchange failed | $e")
            }
        }
    }

    private fun writeTokenToGitConfig(token: String) {
        try {
            val rootFsDir = File(filesDir, "ubuntu_rootfs")
            val rootHomeDir = File(rootFsDir, "root")
            val homeDir = File(rootFsDir, "home")
            val containerHome = File(homeDir, "forgerig")
            val targetHome = if (containerHome.exists()) containerHome else if (rootHomeDir.exists()) rootHomeDir else rootFsDir
            
            if (!targetHome.exists()) {
                targetHome.mkdirs()
            }
            val gitConfigFile = File(targetHome, ".gitconfig")

            val gitConfigContent = """
                [url "https://$token@github.com/"]
                    insteadOf = https://github.com/
            """.trimIndent()

            gitConfigFile.writeText(gitConfigContent)
        } catch (e: Exception) {
            AssetExtractor.logShared(this@MainActivity, "ERROR: guest gitconfig write failed | $e")
        }
    }
}