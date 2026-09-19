package com.forgerig

import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.util.Log
import android.view.Gravity
import android.view.View
import android.view.ViewGroup
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import android.widget.Toast
import androidx.appcompat.app.AppCompatActivity
import okhttp3.*
import org.json.JSONArray
import org.json.JSONObject
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger

/**
 * Manage network allowlist for net_fetch brokered egress.
 * Uses OkHttp WebSocket to call daemon RPCs: network_policy_list/add/remove.
 */
class NetworkAllowlistActivity : AppCompatActivity() {
    private val TAG = "NetworkAllowlist"
    private lateinit var root: LinearLayout
    private var currentScope = "global"
    private var domains: MutableList<String> = mutableListOf()
    private var ws: WebSocket? = null
    private val client = OkHttpClient.Builder()
        .pingInterval(30, TimeUnit.SECONDS)
        .build()
    private val requestId = AtomicInteger(1)
    private val pending = mutableMapOf<Int, (Boolean, String?) -> Unit>()
    private val handler = Handler(Looper.getMainLooper())

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val scroll = ScrollView(this)
        root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(16), dp(16), dp(16), dp(16))
        }
        scroll.addView(root)
        setContentView(scroll)

        root.addView(label("Network Allowlist"))
        root.addView(TextView(this).apply {
            text = "Deny-by-default egress. Only allowlisted domains can be reached via net_fetch."
            setTextColor(0xFF999999.toInt())
            textSize = 13f
            setPadding(0, 0, 0, dp(16))
        })

        // Scope selector
        root.addView(label("Scope"))
        val scopeSpinner = android.widget.Spinner(this).apply {
            adapter = android.widget.ArrayAdapter(this@NetworkAllowlistActivity,
                android.R.layout.simple_spinner_dropdown_item,
                listOf("global", "session:default"))
            setSelection(0)
            onItemSelectedListener = object : android.widget.AdapterView.OnItemSelectedListener {
                override fun onItemSelected(parent: android.widget.AdapterView<*>?, view: android.view.View?, position: Int, id: Long) {
                    currentScope = parent?.getItemAtPosition(position) as String
                    loadDomains()
                }
                override fun onNothingSelected(parent: android.widget.AdapterView<*>?) {}
            }
        }
        root.addView(scopeSpinner)

        // Domain list container
        val listContainer = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            id = View.generateViewId()
        }
        root.addView(listContainer)

        // Add domain row
        val addRow = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            setPadding(0, dp(12), 0, 0)
        }
        val domainEdit = EditText(this).apply {
            hint = "Domain (e.g. api.github.com)"
            setTextColor(0xFFe6e6e6.toInt())
            setHintTextColor(0xFF777777.toInt())
            backgroundTintList = android.content.res.ColorStateList.valueOf(0xFF333742.toInt())
            setPadding(dp(8), dp(8), dp(8), dp(8))
            layoutParams = LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f)
        }
        addRow.addView(domainEdit)
        val addBtn = Button(this).apply {
            text = "Add"
            setBackgroundColor(0xFF9333ea.toInt())
            setTextColor(0xFFFFFFFF.toInt())
            setPadding(dp(16), dp(8), dp(16), dp(8))
            setOnClickListener {
                val domain = domainEdit.text.toString().trim()
                if (domain.isNotBlank()) {
                    addDomain(domain)
                    domainEdit.text.clear()
                }
            }
        }
        addRow.addView(addBtn)
        root.addView(addRow)

        // Connect WebSocket and load initial
        connectWebSocket()

        root.addView(Button(this).apply {
            text = "Done"
            setBackgroundColor(0xFF7f1d1d.toInt())
            setTextColor(0xFFFFFFFF.toInt())
            setOnClickListener { finish() }
        }, LinearLayout.LayoutParams(
            ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT
        ).apply { topMargin = dp(20) })
    }

    private fun connectWebSocket() {
        val port = MainActivity.allocatedPort
        val request = Request.Builder()
            .url("ws://127.0.0.1:$port/")
            .build()
        client.newWebSocket(request, object : WebSocketListener() {
            override fun onOpen(webSocket: WebSocket, response: Response) {
                ws = webSocket
                Log.d(TAG, "WebSocket connected")
                handler.post { loadDomains() }
            }

            override fun onMessage(webSocket: WebSocket, text: String) {
                handler.post {
                    try {
                        val resp = JSONObject(text)
                        val id = resp.optInt("id")
                        val cb = pending.remove(id)
                        if (cb != null) {
                            if (resp.has("error")) {
                                cb(false, resp.getJSONObject("error").optString("message"))
                            } else {
                                cb(true, resp.optString("result"))
                            }
                        }
                    } catch (e: Exception) {
                        Log.e(TAG, "Error parsing response", e)
                    }
                }
            }

            override fun onFailure(webSocket: WebSocket, t: Throwable, response: Response?) {
                Log.e(TAG, "WebSocket error", t)
                handler.postDelayed({ connectWebSocket() }, 2000)
            }
        })
    }

    private fun rpc(method: String, params: JSONObject? = null, cb: (Boolean, String?) -> Unit) {
        val id = requestId.incrementAndGet()
        pending[id] = cb
        val socket = ws
        if (socket == null) {
            pending.remove(id)
            handler.post { cb(false, "not connected") }
            return
        }
        val req = JSONObject().apply {
            put("jsonrpc", "2.0")
            put("method", method)
            put("params", params ?: JSONObject())
            put("id", id)
        }
        socket.send(req.toString())
    }

    private fun loadDomains() {
        rpc("network_policy_list", JSONObject().put("scope", currentScope)) { success, result ->
            if (success) {
                try {
                    val domainsJson = result ?: "{}"
                    val resp = JSONObject(domainsJson)
                    val arr = resp.getJSONArray("domains")
                    val list = mutableListOf<String>()
                    for (i in 0 until arr.length()) {
                        list.add(arr.getString(i))
                    }
                    updateList(list)
                } catch (e: Exception) {
                    Log.e(TAG, "Error parsing domains", e)
                    updateList(emptyList())
                }
            } else {
                Toast.makeText(this, "Failed to load: $result", Toast.LENGTH_SHORT).show()
                updateList(emptyList())
            }
        }
    }

    private fun addDomain(domain: String) {
        val params = JSONObject().apply {
            put("scope", currentScope)
            put("domain", domain)
        }
        rpc("network_policy_add", params) { success, result ->
            handler.post {
                if (success) {
                    Toast.makeText(this@NetworkAllowlistActivity, "Added $domain", Toast.LENGTH_SHORT).show()
                    loadDomains()
                } else {
                    Toast.makeText(this@NetworkAllowlistActivity, "Failed: $result", Toast.LENGTH_SHORT).show()
                }
            }
        }
    }

    private fun removeDomain(domain: String) {
        val params = JSONObject().apply {
            put("scope", currentScope)
            put("domain", domain)
        }
        rpc("network_policy_remove", params) { success, result ->
            handler.post {
                if (success) {
                    Toast.makeText(this@NetworkAllowlistActivity, "Removed $domain", Toast.LENGTH_SHORT).show()
                    loadDomains()
                } else {
                    Toast.makeText(this@NetworkAllowlistActivity, "Failed: $result", Toast.LENGTH_SHORT).show()
                }
            }
        }
    }

    private fun updateList(newDomains: List<String>) {
        domains = newDomains.toMutableList()
        val listContainer = findListContainer() ?: return
        listContainer.removeAllViews()

        if (newDomains.isEmpty()) {
            listContainer.addView(TextView(this).apply {
                text = "No domains allowlisted. Add one above."
                setTextColor(0xFF777777.toInt())
                textSize = 13f
                setPadding(0, dp(16), 0, 0)
                gravity = Gravity.CENTER
            })
            return
        }

        for (domain in newDomains) {
            val row = LinearLayout(this).apply {
                orientation = LinearLayout.HORIZONTAL
                gravity = Gravity.CENTER_VERTICAL
                setPadding(0, dp(8), 0, dp(8))
            }
            row.addView(TextView(this).apply {
                text = domain
                textSize = 14f
                setTextColor(0xFFe6e6e6.toInt())
                layoutParams = LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f)
            })
            row.addView(Button(this).apply {
                text = "Remove"
                setBackgroundColor(0xFF7f1d1d.toInt())
                setTextColor(0xFFFFFFFF.toInt())
                setPadding(dp(12), dp(4), dp(12), dp(4))
                setOnClickListener { removeDomain(domain) }
            })
            listContainer.addView(row)
        }
    }

    private fun findListContainer(): LinearLayout? {
        // The domain list is the LinearLayout created with a generated view id.
        for (i in 0 until root.childCount) {
            val v = root.getChildAt(i)
            if (v is LinearLayout && v.id != View.NO_ID) return v
        }
        return null
    }

    override fun onDestroy() {
        ws?.cancel()
        super.onDestroy()
    }

    private fun label(text: String): TextView = TextView(this).apply {
        this.text = text
        setPadding(0, dp(14), 0, dp(4))
        textSize = 14f
        setTextColor(0xFFe6e6e6.toInt())
    }

    private fun dp(n: Int): Int = (n * resources.displayMetrics.density).toInt()
}