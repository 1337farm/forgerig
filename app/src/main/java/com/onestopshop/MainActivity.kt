package com.onestopshop

import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.webkit.JavascriptInterface
import android.webkit.WebView
import android.webkit.WebViewClient
import androidx.appcompat.app.AppCompatActivity
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
                            .spinner { margin: 20px auto; width: 40px; height: 40px; border: 4px solid #f3f3f3; border-top: 4px solid #3498db; border-radius: 50%; animation: spin 1s linear infinite; }
                            @keyframes spin { 0% { transform: rotate(0deg); } 100% { transform: rotate(360deg); } }
                            .btn { background-color: #3498db; border: none; color: white; padding: 15px 32px; text-align: center; text-decoration: none; display: inline-block; font-size: 16px; margin: 4px 2px; cursor: pointer; border-radius: 8px; }
                            .btn:disabled { background-color: #9bb8d0; cursor: default; }
                            .progress-track { margin: 20px 0 8px; height: 12px; background-color: #e0e0e0; border-radius: 6px; overflow: hidden; display: none; }
                            .progress-fill { height: 100%; width: 0; background-color: #3498db; border-radius: 6px; transition: width 0.2s ease; }
                            .stage { margin: 4px 0; font-size: 14px; color: #555; display: none; }
                            .detail { margin: 4px 0 8px; font-size: 12px; color: #999; display: none; word-break: break-all; text-align: left; background: #fafafa; padding: 6px; border-radius: 4px; max-height: 100px; overflow-y: auto; }
                            .error { margin: 12px 0; padding: 10px; border-radius: 6px; background-color: #fdecea; color: #c0392b; font-size: 14px; display: none; word-break: break-all; text-align: left; }
                            .options { margin-top: 12px; font-size: 13px; color: #666; text-align: left; }
                        </style>
                        <script>
                            var installState = null;

                            function storeEnabled() {
                                try {
                                    window.localStorage.setItem('verbose', '0');
                                    window.localStorage.removeItem('verbose');
                                    return true;
                                } catch (e) {
                                    return false;
                                }
                            }
                            var canStore = storeEnabled();
                            var verboseFallback = false;

                            function getVerbose() {
                                if (canStore) {
                                    return window.localStorage.getItem('verbose') === '1';
                                }
                                return verboseFallback;
                            }

                            function setVerbose(v) {
                                if (canStore) {
                                    window.localStorage.setItem('verbose', v ? '1' : '0');
                                } else {
                                    verboseFallback = v;
                                }
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

                            function renderInstall() {
                                if (!installState) { setTimeout(pollInstall, 300); return; }

                                var phase = installState.phase;
                                var bar = document.getElementById('progress-fill');
                                var track = document.getElementById('progress-track');
                                var stage = document.getElementById('stage-text');
                                var detail = document.getElementById('detail-text');
                                var err = document.getElementById('error-text');
                                var connect = document.getElementById('connecting');
                                var installBtn = document.getElementById('install-btn');
                                var spinner = document.getElementById('spinner');

                                if (phase === 'installing') {
                                    installBtn.style.display = 'none';
                                    installBtn.disabled = true;
                                    track.style.display = 'block';
                                    stage.style.display = 'block';
                                    detail.style.display = getVerbose() ? 'block' : 'none';
                                    connect.style.display = 'block';
                                    spinner.style.display = 'block';
                                    err.style.display = 'none';
                                    bar.style.width = installState.percent + '%';
                                    stage.innerText = installState.stage + ' ' + installState.percent + '%';
                                    detail.innerText = installState.detail;
                                } else if (phase === 'starting' || phase === 'ready') {
                                    installBtn.style.display = 'none';
                                    installBtn.disabled = true;
                                    track.style.display = 'block';
                                    stage.style.display = 'block';
                                    detail.style.display = 'none';
                                    connect.style.display = 'block';
                                    spinner.style.display = 'block';
                                    bar.style.width = '100%';
                                    stage.innerText = phase === 'ready' ? 'Environment ready, connecting…' : 'Environment starting…';
                                    setTimeout(function() { window.location.reload(); }, 3000);
                                } else if (phase === 'failed') {
                                    installBtn.style.display = 'inline-block';
                                    installBtn.disabled = false;
                                    document.getElementById('copy-btn').style.display = 'inline-block';
                                    track.style.display = 'none';
                                    stage.style.display = 'none';
                                    detail.style.display = 'none';
                                    connect.style.display = 'none';
                                    spinner.style.display = 'none';
                                    err.style.display = 'block';
                                    if (getVerbose() && installState.errorDetail) {
                                        err.innerText = installState.error + '\n\n' + installState.errorDetail;
                                    } else {
                                        err.innerText = installState.error;
                                    }
                                } else {
                                    installBtn.style.display = 'inline-block';
                                    installBtn.disabled = false;
                                    track.style.display = 'none';
                                    stage.style.display = 'none';
                                    detail.style.display = 'none';
                                    spinner.style.display = 'none';
                                    connect.style.display = 'none';
                                }
                            }

                            function copyLogs() {
                                var text = "";
                                if (installState) {
                                    text = "Phase: " + installState.phase + "\nStage: " + installState.stage + "\nDetail: " + installState.detail + "\nError: " + installState.error + "\nErrorDetail: " + installState.errorDetail;
                                } else {
                                    text = document.getElementById('error-text').innerText || document.getElementById('stage-text').innerText;
                                }
                                navigator.clipboard.writeText(text).then(function() {
                                    alert("Logs copied to clipboard!");
                                }, function(err) {
                                    alert("Failed to copy logs: " + err);
                                });
                            }

                            function install() {
                                if (window.NativeHost) {
                                    document.getElementById('install-btn').disabled = true;
                                    document.getElementById('connecting').style.display = 'block';
                                    document.getElementById('spinner').style.display = 'block';
                                    document.getElementById('progress-track').style.display = 'block';
                                    document.getElementById('stage-text').style.display = 'block';
                                    document.getElementById('stage-text').innerText = 'Starting installation…';
                                    window.NativeHost.installNow();
                                }
                            }

                            window.onload = function() {
                                document.getElementById('verbose').checked = getVerbose();
                                pollInstall();
                            };
                        </script>
                    </head>
                    <body>
                        <div class="message">
                            <h2 id="title">ForgeRig</h2>
                            <div id="connecting" style="display:none;">
                                <h3>Connecting to Container...</h3>
                                <div class="spinner" id="spinner"></div>
                            </div>
                            <div class="progress-track" id="progress-track"><div class="progress-fill" id="progress-fill"></div></div>
                            <p class="stage" id="stage-text"></p>
                            <p class="detail" id="detail-text"></p>
                            <div class="error" id="error-text"></div>
                            <button id="install-btn" class="btn" style="display:none;" onclick="install()">Install Environment</button>
                            <br><button id="copy-btn" class="btn" style="background-color: #7f8c8d; margin-top: 8px; font-size: 14px; padding: 10px 20px; display: none;" onclick="copyLogs()">Copy Logs</button>
                            <div class="options">
                                <label><input type="checkbox" id="verbose" onchange="onVerboseChanged()"> Show detailed progress</label>
                            </div>
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

        @JavascriptInterface
        fun getInstallState(): String {
            return JSONObject()
                .put("phase", phase)
                .put("percent", percent)
                .put("stage", stage)
                .put("detail", detail)
                .put("error", error)
                .put("errorDetail", errorDetail)
                .toString()
        }

        @JavascriptInterface
        fun installNow() {
            if (phase == "installing") {
                return
            }
            phase = "installing"
            percent = 0
            stage = "Preparing…"
            detail = ""
            error = ""
            errorDetail = ""

            AssetExtractor(context)
                .setProgressListener(object : InstallProgress {
                    override fun onProgress(p: Int, s: String, d: String) {
                        percent = p
                        stage = s
                        detail = d
                    }

                    override fun onError(message: String, detailText: String) {
                        phase = "failed"
                        error = message
                        errorDetail = detailText
                    }

                    override fun onDone() {
                        phase = "ready"
                        startContainer()
                    }
                })
                .extractAssets()
        }

        @JavascriptInterface
        fun startContainer() {
            // Start foreground service
            val serviceIntent = Intent(context, ContainerService::class.java)
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                context.startForegroundService(serviceIntent)
            } else {
                context.startService(serviceIntent)
            }
        }

        @JavascriptInterface
        fun isInstalled(): Boolean {
            val targetDir = File(context.filesDir, "ubuntu_rootfs")
            val prootFile = File(targetDir, "proot")
            return prootFile.exists()
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
                e.printStackTrace()
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
            e.printStackTrace()
        }
    }
}