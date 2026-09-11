package com.onestopshop

import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.webkit.JavascriptInterface
import android.webkit.WebView
import android.webkit.WebViewClient
import androidx.appcompat.app.AppCompatActivity
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
    }

    private lateinit var webView: WebView

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_main)

        // Initialize WebView
        webView = findViewById(R.id.webView)
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
                        <style>
                            body { font-family: sans-serif; display: flex; justify-content: center; align-items: center; height: 100vh; margin: 0; background-color: #f0f0f0; }
                            .message { text-align: center; padding: 20px; background: white; border-radius: 8px; box-shadow: 0 4px 6px rgba(0,0,0,0.1); width: 84%; max-width: 420px; }
                            .btn { background-color: #3498db; border: none; color: white; padding: 15px 32px; text-align: center; text-decoration: none; display: inline-block; font-size: 16px; margin: 4px 2px; cursor: pointer; border-radius: 8px; }
                            .btn:disabled { background-color: #9bb8d0; cursor: default; }
                            .progress-track { margin: 12px 0 8px; height: 12px; background-color: #e0e0e0; border-radius: 6px; overflow: hidden; display: none; }
                            .progress-fill { height: 100%; width: 0; background-color: #3498db; border-radius: 6px; transition: width 0.2s ease; }
                            .step-count { margin: 8px 0 4px; font-size: 14px; font-weight: bold; color: #333; display: none; }
                            .steps { list-style: none; margin: 8px 0; padding: 0; text-align: left; display: none; }
                            .steps li { margin: 6px 0; font-size: 14px; color: #bbb; }
                            .steps li.done { color: #27ae60; text-decoration: line-through; }
                            .steps li.active { color: #111; font-weight: bold; }
                            .steps li.failed { color: #c0392b; font-weight: bold; }
                            .steps .dot { display: inline-block; width: 10px; height: 10px; margin-right: 6px; border-radius: 50%; background-color: #3498db; animation: pulse 1s infinite; }
                            @keyframes pulse { 50% { opacity: 0.25; } }
                            .detail { margin: 4px 0 8px; font-size: 12px; color: #999; display: none; word-break: break-all; text-align: left; background: #fafafa; padding: 6px; border-radius: 4px; max-height: 100px; overflow-y: auto; }
                            .error { margin: 12px 0; padding: 10px; border-radius: 6px; background-color: #fdecea; color: #c0392b; font-size: 14px; display: none; word-break: break-all; text-align: left; }
                        </style>
                        <script>
                            var installState = null;

                            var reloading = false;
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
                                    installBtn.style.display = 'none';
                                    copyBtn.style.display = 'none';
                                    err.style.display = 'none';
                                    track.style.display = 'none';
                                    renderSteps(false);
                                    steps.style.display = 'block';
                                    count.style.display = 'block';
                                    count.innerText = stepsTotal() + ' of ' + stepsTotal() + ' steps complete — opening workspace…';
                                    detail.style.display = 'none';
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

        @JavascriptInterface
        fun installNow() {
            if (phase == "installing") {
                return
            }
            phase = "installing"
            percent = 0
            step = 0
            stage = "Preparing…"
            detail = ""
            error = ""
            errorDetail = ""

            AssetExtractor(context)
                .setProgressListener(object : InstallProgress {
                    override fun onProgress(percent: Int, stage: String, detail: String) {
                        this@WebAppInterface.percent = percent
                        this@WebAppInterface.stage = stage
                        this@WebAppInterface.detail = detail
                    }

                    override fun onStep(step: Int) {
                        this@WebAppInterface.step = step
                    }

                    override fun onError(message: String, detail: String) {
                        phase = "failed"
                        error = message
                        errorDetail = detail
                    }

                    override fun onDone() {
                        launchContainerAndWait()
                    }
                })
                .extractAssets()
        }

        private fun startContainerInternal() {
            val serviceIntent = Intent(context, ContainerService::class.java)
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                context.startForegroundService(serviceIntent)
            } else {
                context.startService(serviceIntent)
            }
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
                    return@thread
                }
                step = 4
                stage = "Connecting to daemon…"
                val maxAttempts = 30
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
                    Thread.sleep(2000)
                }
                if (ready) {
                    AssetExtractor.logShared(context, "DONE: Container ready, opening workspace")
                    percent = 100
                    phase = "ready"
                } else if (phase == "installing") {
                    AssetExtractor.logShared(context, "ERROR: Container did not respond in 60s after $maxAttempts attempts")
                    phase = "failed"
                    error = "Container did not respond in 60s"
                    errorDetail = "Timed out after $maxAttempts attempts probing " +
                        "http://127.0.0.1:${MainActivity.allocatedPort}/. The service started but " +
                        "nothing serves HTTP. See Downloads/${AssetExtractor.sharedLogFileName()} for details."
                }
            }
        }
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        handleIntent(intent)
    }

    private fun handleIntent(intent: Intent?) {
        val action = intent?.action
        val data: Uri? = intent?.data

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