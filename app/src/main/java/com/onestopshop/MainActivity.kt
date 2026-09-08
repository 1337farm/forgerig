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
                            .message { text-align: center; padding: 20px; background: white; border-radius: 8px; box-shadow: 0 4px 6px rgba(0,0,0,0.1); }
                            .spinner { margin: 20px auto; width: 40px; height: 40px; border: 4px solid #f3f3f3; border-top: 4px solid #3498db; border-radius: 50%; animation: spin 1s linear infinite; }
                            @keyframes spin { 0% { transform: rotate(0deg); } 100% { transform: rotate(360deg); } }
                            .btn { background-color: #3498db; border: none; color: white; padding: 15px 32px; text-align: center; text-decoration: none; display: inline-block; font-size: 16px; margin: 4px 2px; cursor: pointer; border-radius: 8px; }
                        </style>
                        <script>
                            function checkStatus() {
                                if (window.NativeHost && window.NativeHost.isInstalled()) {
                                    window.NativeHost.startContainer();
                                    document.getElementById('install-btn').style.display = 'none';
                                    document.getElementById('connecting').style.display = 'block';
                                    setTimeout(function() { window.location.reload(); }, 3000);
                                } else {
                                    document.getElementById('install-btn').style.display = 'inline-block';
                                    document.getElementById('connecting').style.display = 'none';
                                }
                            }

                            function install() {
                                if (window.NativeHost) {
                                    document.getElementById('install-btn').style.display = 'none';
                                    document.getElementById('connecting').style.display = 'block';
                                    document.getElementById('status-text').innerText = 'Installing and starting environment...';

                                    setTimeout(function() {
                                        window.NativeHost.installNow();
                                        setTimeout(function() { window.location.reload(); }, 3000);
                                    }, 100);
                                }
                            }
                            window.onload = checkStatus;
                        </script>
                    </head>
                    <body>
                        <div class="message">
                            <button id="install-btn" class="btn" style="display:none;" onclick="install()">Install Environment</button>
                            <div id="connecting" style="display:none;">
                                <h2>Connecting to Container...</h2>
                                <div class="spinner"></div>
                                <p id="status-text">Please wait while the environment starts.</p>
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
            @JavascriptInterface
            fun installNow() {
                val extractor = AssetExtractor(context)
                extractor.setProgressListener(object : AssetExtractor.ProgressListener {
                    override fun onProgress(message: String, percentage: Int) {
                        context.runOnUiThread {
                            val js = if (percentage < 100) {
                                """
                                    document.getElementById('install-btn').style.display = 'none';
                                    document.getElementById('connecting').style.display = 'block';
                                    document.getElementById('status-text').innerText = '$message $percentage%';
                                """.trimIndent()
                            } else {
                                """
                                    document.getElementById('connecting').style.display = 'none';
                                    document.getElementById('status-text').innerText = 'Container starting...';
                                """.trimIndent()
                            }
                            context.webView.evaluateJavascript(js, null)
                        }
                    }

                    override fun onComplete() {
                        context.runOnUiThread {
                            val js = """
                                document.getElementById('connecting').style.display = 'none';
                            """.trimIndent()
                            context.webView.evaluateJavascript(js, null)
                        }
                        thread {
                    val serviceIntent = Intent(context, ContainerService::class.java)
                    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                        context.startForegroundService(serviceIntent)
                    } else {
                        context.startService(serviceIntent)
                    }
                }
                    }

                    override fun onError(error: String) {
                        context.runOnUiThread {
                            val js = """
                                document.getElementById('status-text').innerText = 'Installation failed: $error';
                            """.trimIndent()
                            context.webView.evaluateJavascript(js, null)
                        }
                    }
                })
                extractor.extractAssets()
            }

            @JavascriptInterface
            fun startContainer() {
                val targetDir = File(context.filesDir, "ubuntu_rootfs")
                val prootFile = File(targetDir, "proot")
                if (!prootFile.exists()) {
                    context.runOnUiThread {
                        val js = """
                            document.getElementById('status-text').innerText = 'Assets not found. Please run install again.';
                        """.trimIndent()
                        context.webView.evaluateJavascript(js, null)
                    }
                    return
                }

                // Start foreground service
                val serviceIntent = Intent(context, ContainerService::class.java)
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                    context.startForegroundService(serviceIntent)
                } else {
                    context.startService(serviceIntent)
                }

                // Show connecting state
                context.runOnUiThread {
                    val js = """
                        document.getElementById('connecting').style.display = 'block';
                        document.getElementById('status-text').innerText = 'Starting container...';
                    """.trimIndent()
                    context.webView.evaluateJavascript(js, null)
                }

                context.startContainer()
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
            if (!rootHomeDir.exists()) {
                rootHomeDir.mkdirs()
            }
            val gitConfigFile = File(rootHomeDir, ".gitconfig")

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
