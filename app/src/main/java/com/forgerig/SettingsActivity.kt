package com.forgerig

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.res.ColorStateList
import android.os.Build
import android.os.Bundle
import android.view.View
import android.view.ViewGroup
import android.widget.ArrayAdapter
import android.widget.Button
import android.widget.CheckBox
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.Spinner
import android.widget.TextView
import android.widget.Toast
import androidx.appcompat.app.AppCompatDelegate
import androidx.appcompat.app.AppCompatActivity

class SettingsActivity : AppCompatActivity() {

    private val providerOptions = listOf(
        "openai - OpenAI (gpt-4o-mini)" to "openai",
        "openrouter - OpenRouter (free Llama)" to "openrouter",
        "nvidia - NVIDIA NIM (free credits)" to "nvidia",
        "groq - Groq (free tier)" to "groq",
        "deepseek - DeepSeek (cheap)" to "deepseek",
        "mistral - Mistral" to "mistral",
        "gemini - Google Gemini (free tier)" to "gemini",
        "ollama - Local LLM" to "ollama",
        "custom - any OpenAI-compatible endpoint" to "custom",
    )

    private data class ModelOption(val name: String, val free: Boolean)

    // Curated real model IDs per provider (free flags follow each provider's
    // free tier). "Refresh from provider" below merges the live /v1/models
    // list so drift self-heals; curated entries are the offline fallback.
    private val modelCatalog: Map<String, List<ModelOption>> = mapOf(
        "openai" to listOf(
            ModelOption("gpt-4o-mini", false),
            ModelOption("gpt-4o", false),
        ),
        "openrouter" to listOf(
            ModelOption("meta-llama/llama-3.3-70b-instruct:free", true),
            ModelOption("google/gemma-3-27b-it:free", true),
            ModelOption("deepseek/deepseek-chat-v3-0324:free", true),
        ),
        "nvidia" to listOf(
            ModelOption("nvidia/llama-3.1-nemotron-70b-instruct", true),
            ModelOption("meta/llama-3.1-405b-instruct", true),
            ModelOption("mistralai/mixtral-8x22b-instruct-v0.1", true),
            ModelOption("deepseek-ai/deepseek-r1", true),
        ),
        "groq" to listOf(
            ModelOption("llama-3.3-70b-versatile", true),
            ModelOption("llama-3.1-8b-instant", true),
            ModelOption("mixtral-8x7b-32768", true),
            ModelOption("gemma2-9b-it", true),
        ),
        "deepseek" to listOf(
            ModelOption("deepseek-chat", false),
            ModelOption("deepseek-reasoner", false),
        ),
        "mistral" to listOf(
            ModelOption("mistral-small-latest", false),
            ModelOption("mistral-medium-latest", false),
            ModelOption("mistral-large-latest", false),
            ModelOption("open-mistral-7b", false),
            ModelOption("open-mixtral-8x7b", false),
        ),
        "gemini" to listOf(
            ModelOption("gemini-2.5-flash", true),
            ModelOption("gemini-2.5-pro", true),
            ModelOption("gemini-2.0-flash", true),
        ),
        "ollama" to listOf(
            ModelOption("llama3.1:8b", true),
            ModelOption("llama3.1:70b", true),
            ModelOption("mistral", true),
            ModelOption("gemma2", true),
            ModelOption("qwen2.5", true),
            ModelOption("deepseek-r1", true),
        ),
        "custom" to emptyList(),
    )

    private val defaultBaseUrls = mapOf(
        "openai" to "https://api.openai.com",
        "openrouter" to "https://openrouter.ai/api",
        "nvidia" to "https://integrate.api.nvidia.com",
        "groq" to "https://api.groq.com/openai",
        "deepseek" to "https://api.deepseek.com",
        "mistral" to "https://api.mistral.ai",
        "gemini" to "https://generativelanguage.googleapis.com",
        "ollama" to "http://localhost:11434",
    )

    // Live-fetched model IDs merged over the curated catalog (per provider).
    private val extraModels: MutableMap<String, MutableList<String>> = mutableMapOf()

    private val finishReceiver = object : BroadcastReceiver() {
        override fun onReceive(context: Context?, intent: Intent?) {
            if (intent?.action == ContainerService.ACTION_FINISH_APP) {
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
        AppCompatDelegate.setDefaultNightMode(AppCompatDelegate.MODE_NIGHT_YES)
        super.onCreate(savedInstanceState)
        // The default action bar is light-gray and clashes with the neon
        // theme: hide it and draw our own title in-layout instead.
        supportActionBar?.hide()

        registerFinishReceiver()

        val current = SettingsStore.load(this)

        val scroll = ScrollView(this)
        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(16), dp(16), dp(16), dp(16))
        }
        scroll.addView(root)
        setContentView(scroll)

        root.addView(label("ForgeRig Settings"))
        root.addView(TextView(this).apply {
            text = "Provider, model, and API key are encrypted with the Android Keystore. " +
                "Keys are never written to plaintext files or sent to the model."
            setTextColor(0xFF999999.toInt())
            textSize = 13f
            setPadding(0, 0, 0, dp(16))
        })

        fun label(text: String): TextView = TextView(this).apply {
            this.text = text
            setPadding(0, dp(14), 0, dp(4))
            textSize = 14f
            setTextColor(0xFFe6e6e6.toInt())
        }

        fun editText(text: String, hint: String): EditText = EditText(this).apply {
            setText(text)
            this.hint = hint
            setTextColor(0xFFe6e6e6.toInt())
            setHintTextColor(0xFF777777.toInt())
            backgroundTintList = ColorStateList.valueOf(0xFF333742.toInt())
            setPadding(dp(8), dp(8), dp(8), dp(8))
        }

        root.addView(label("Provider"))
        val spinner = Spinner(this).apply {
            adapter = spinnerAdapter().apply {
                addAll(providerOptions.map { it.first })
            }
            val idx = providerOptions.indexOfFirst { it.second == current.provider }
            setSelection(if (idx >= 0) idx else 0)
        }
        root.addView(spinner)

        root.addView(label("Model (leave blank for provider default)"))
        val modelEdit = editText(current.model, "e.g. mistral-small-latest or llama-3.3-70b-versatile")
        val freeOnlyBox = CheckBox(this).apply {
            text = "Show free models only"
            isChecked = true
            setTextColor(0xFFe6e6e6.toInt())
        }
        root.addView(freeOnlyBox)
        val modelFilterEdit = editText("", "Filter models by name…")
        root.addView(modelFilterEdit)
        val modelSpinner = Spinner(this).apply {
            adapter = spinnerAdapter()
        }
        root.addView(modelSpinner)
        root.addView(modelEdit)
        val evalSpinner = Spinner(this).apply {
            adapter = spinnerAdapter()
        }
        var refreshingModels = false
        fun fillSpinner(view: Spinner, names: List<String>, selected: String) {
            @Suppress("UNCHECKED_CAST")
            val adapter = view.adapter as ArrayAdapter<String>
            adapter.clear()
            adapter.addAll(names)
            adapter.notifyDataSetChanged()
            val idx = names.indexOfFirst { it.removeSuffix("  (free)") == selected }
            if (idx >= 0) view.setSelection(idx)
        }
        fun refreshModels() {
            if (refreshingModels) return
            refreshingModels = true
            try {
                val provider = providerOptions[spinner.selectedItemPosition].second
                val query = modelFilterEdit.text.toString().trim().lowercase()
                val known = (modelCatalog[provider] ?: emptyList()).toMutableList()
                for (extra in extraModels[provider] ?: emptyList()) {
                    if (known.none { it.name == extra }) known.add(ModelOption(extra, extra.endsWith(":free")))
                }
                val names = known
                    .filter { (!freeOnlyBox.isChecked || it.free) && (query.isEmpty() || it.name.lowercase().contains(query)) }
                    .map { if (it.free) "${it.name}  (free)" else it.name }
                    .sorted()
                fillSpinner(modelSpinner, names, current.model)
                fillSpinner(evalSpinner, names, current.evalModel)
            } finally {
                refreshingModels = false
            }
        }
        spinner.onItemSelectedListener = object : android.widget.AdapterView.OnItemSelectedListener {
            override fun onItemSelected(parent: android.widget.AdapterView<*>?, view: android.view.View?, position: Int, id: Long) = refreshModels()
            override fun onNothingSelected(parent: android.widget.AdapterView<*>?) {}
        }
        modelSpinner.onItemSelectedListener = object : android.widget.AdapterView.OnItemSelectedListener {
            override fun onItemSelected(parent: android.widget.AdapterView<*>?, view: android.view.View?, position: Int, id: Long) {
                if (refreshingModels) return
                val item = parent?.getItemAtPosition(position) as? String ?: return
                modelEdit.setText(item.removeSuffix("  (free)"))
            }
            override fun onNothingSelected(parent: android.widget.AdapterView<*>?) {}
        }
        freeOnlyBox.setOnCheckedChangeListener { _, _ -> refreshModels() }
        modelFilterEdit.addTextChangedListener(object : android.text.TextWatcher {
            override fun beforeTextChanged(s: CharSequence?, start: Int, count: Int, after: Int) {}
            override fun onTextChanged(s: CharSequence?, start: Int, before: Int, after: Int) = refreshModels()
            override fun afterTextChanged(s: android.text.Editable?) {}
        })
        root.addView(label("Evaluation model (blank = same as chat)"))
        root.addView(evalSpinner)
        val evalEdit = editText(current.evalModel, "cheap model for background memory evaluation")
        root.addView(evalEdit)
        root.addView(label("Base URL (blank = provider default; required for custom)"))
        val urlEdit = editText(current.baseUrl, "https://host/api (OpenAI-compatible)")
        root.addView(urlEdit)
        root.addView(label("API key"))
        val keyEdit = editText(current.apiKey, "stored encrypted on this device")
        root.addView(keyEdit)
        root.addView(label("Max output tokens (blank = provider default)"))
        val maxTokensEdit = editText(current.maxTokens, "e.g. 2000").apply {
            setInputType(android.text.InputType.TYPE_CLASS_NUMBER)
        }
        root.addView(maxTokensEdit)

        evalSpinner.onItemSelectedListener = object : android.widget.AdapterView.OnItemSelectedListener {
            override fun onItemSelected(parent: android.widget.AdapterView<*>?, view: android.view.View?, position: Int, id: Long) {
                if (refreshingModels) return
                val item = parent?.getItemAtPosition(position) as? String ?: return
                evalEdit.setText(item.removeSuffix("  (free)"))
            }
            override fun onNothingSelected(parent: android.widget.AdapterView<*>?) {}
        }

        fun fetchModelsFor(provider: String, base: String, key: String): List<String> {
            if (provider == "custom" && base.isEmpty()) return emptyList()
            val (url, auth) = when (provider) {
                "gemini" -> "${base.trimEnd('/')}/v1beta/models?key=$key" to null
                "ollama" -> "${base.trimEnd('/')}/api/tags" to null
                else -> "${base.trimEnd('/')}/v1/models" to key.ifEmpty { null }
            }
            val conn = java.net.URL(url).openConnection() as java.net.HttpURLConnection
            try {
                conn.connectTimeout = 15000
                conn.readTimeout = 15000
                if (auth != null) conn.setRequestProperty("Authorization", "Bearer $auth")
                if (conn.responseCode !in 200..299) throw java.io.IOException("HTTP ${conn.responseCode}")
                val body = conn.inputStream.bufferedReader().readText()
                val json = org.json.JSONObject(body)
                val ids = mutableListOf<String>()
                if (provider == "ollama") {
                    val arr = json.optJSONArray("models") ?: org.json.JSONArray()
                    for (i in 0 until arr.length()) {
                        arr.optJSONObject(i)?.optString("name")?.takeIf { it.isNotEmpty() }?.let { ids.add(it) }
                    }
                } else if (provider == "gemini") {
                    val arr = json.optJSONArray("models") ?: org.json.JSONArray()
                    for (i in 0 until arr.length()) {
                        arr.optJSONObject(i)?.optString("name")?.removePrefix("models/")?.takeIf { it.isNotEmpty() }?.let { ids.add(it) }
                    }
                } else {
                    val arr = json.optJSONArray("data") ?: org.json.JSONArray()
                    for (i in 0 until arr.length()) {
                        arr.optJSONObject(i)?.optString("id")?.takeIf { it.isNotEmpty() }?.let { ids.add(it) }
                    }
                }
                return ids
            } finally {
                conn.disconnect()
            }
        }

        fun fetchModels() {
            val current = providerOptions[spinner.selectedItemPosition].second
            Toast.makeText(this, "Refreshing all providers…", Toast.LENGTH_SHORT).show()
            Thread {
                try {
                    val key = keyEdit.text.toString().trim()
                    val customBase = urlEdit.text.toString().trim()
                    var totalAdded = 0
                    var totalSeen = 0
                    val failures = mutableListOf<String>()
                    for ((_, code) in providerOptions) {
                        // Each provider is queried against its own default base
                        // (custom keeps the typed URL); one provider's outage
                        // must not abort the rest.
                        val base = if (code == "custom") customBase else defaultBaseUrls[code] ?: ""
                        if (base.isEmpty()) continue
                        try {
                            val ids = fetchModelsFor(code, base, key)
                            totalSeen += ids.size
                            val known = extraModels.getOrPut(code) { mutableListOf() }
                            for (id in ids.sorted()) {
                                if ((modelCatalog[code] ?: emptyList()).none { it.name == id } && !known.contains(id)) {
                                    known.add(id)
                                    totalAdded++
                                }
                            }
                        } catch (e: Exception) {
                            failures.add("$code: ${e.message}")
                            AssetExtractor.logShared(this, "WARNING: model refresh failed for $code | $e")
                        }
                    }
                    runOnUiThread {
                        refreshModels()
                        val tail = if (failures.isEmpty()) "" else " Failures: ${failures.joinToString("; ")}"
                        Toast.makeText(this, "Added $totalAdded model(s), $totalSeen seen across providers.$tail", Toast.LENGTH_LONG).show()
                    }
                } catch (e: Exception) {
                    runOnUiThread {
                        Toast.makeText(this, "Refresh failed: ${e.message}", Toast.LENGTH_LONG).show()
                    }
                }
            }.start()
        }
        root.addView(Button(this).apply {
            text = "Refresh models (all providers)"
            setBackgroundColor(0xFF3a3348.toInt())
            setTextColor(0xFFe6e6e6.toInt())
            setOnClickListener { fetchModels() }
        })
        refreshModels()

        // Network Allowlist section
        root.addView(label("Network Allowlist (for net_fetch)"))
        val netAllowBtn = Button(this).apply {
            text = "Manage Allowlist…"
            setBackgroundColor(0xFF1e3a5f.toInt())
            setTextColor(0xFFFFFFFF.toInt())
            setOnClickListener {
                startActivity(Intent(this@SettingsActivity, NetworkAllowlistActivity::class.java))
            }
        }
        root.addView(netAllowBtn, LinearLayout.LayoutParams(
            ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT
        ).apply { topMargin = dp(8) })

        root.addView(Button(this).apply {
            text = "Save"
            setBackgroundColor(0xFF9333ea.toInt())
            setTextColor(0xFFFFFFFF.toInt())
            setOnClickListener {
                try {
                    SettingsStore.save(
                        this@SettingsActivity,
                        Settings(
                            provider = providerOptions[spinner.selectedItemPosition].second,
                            model = modelEdit.text.toString(),
                            evalModel = evalEdit.text.toString(),
                            baseUrl = urlEdit.text.toString(),
                            apiKey = keyEdit.text.toString(),
                            maxTokens = maxTokensEdit.text.toString(),
                        ),
                    )
                } catch (e: Exception) {
                    AssetExtractor.logShared(this@SettingsActivity, "ERROR: settings save refused (fail-closed) | $e")
                    Toast.makeText(this@SettingsActivity, "Secure storage unavailable — key NOT saved.", Toast.LENGTH_LONG).show()
                    return@setOnClickListener
                }
                try {
                    val restart = Intent(this@SettingsActivity, ContainerService::class.java)
                        .setAction(ContainerService.ACTION_RESTART)
                    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                        this@SettingsActivity.startForegroundService(restart)
                    } else {
                        this@SettingsActivity.startService(restart)
                    }
                    Toast.makeText(this@SettingsActivity, "Saved — restarting container with new settings…", Toast.LENGTH_LONG).show()
                } catch (e: Exception) {
                    AssetExtractor.logShared(this@SettingsActivity, "ERROR: restart container failed | $e")
                    Toast.makeText(this@SettingsActivity, "Saved — restart the container to apply.", Toast.LENGTH_LONG).show()
                }
                finish()
            }
        }, LinearLayout.LayoutParams(
            ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT
        ).apply { topMargin = dp(20) })

        root.addView(Button(this).apply {
            text = "Stop container"
            setBackgroundColor(0xFF7f1d1d.toInt())
            setTextColor(0xFFFFFFFF.toInt())
            setOnClickListener {
                try {
                    val stop = Intent(this@SettingsActivity, ContainerService::class.java)
                        .setAction(ContainerService.ACTION_STOP)
                    startService(stop)
                } catch (e: Exception) {
                    AssetExtractor.logShared(this@SettingsActivity, "ERROR: stop container failed | $e")
                }
                Toast.makeText(this@SettingsActivity, "Container stop requested.", Toast.LENGTH_SHORT).show()
                finish()
            }
        }, LinearLayout.LayoutParams(
            ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT
        ).apply { topMargin = dp(12) })

        root.addView(TextView(this).apply {
            text = "The API key is encrypted with the Android keystore and only injected into the container at launch — it never reaches the model through chat."
            setPadding(0, dp(14), 0, 0)
            textSize = 12f
            setTextColor(0xFF777777.toInt())
        })
    }

    override fun onStart() {
        super.onStart()
        registerFinishReceiver()
    }

    override fun onStop() {
        super.onStop()
        try { unregisterReceiver(finishReceiver) } catch (_: Exception) {}
    }

    private fun label(text: String): TextView = TextView(this).apply {
        this.text = text
        setPadding(0, dp(14), 0, dp(4))
        textSize = 14f
        setTextColor(0xFFe6e6e6.toInt())
    }

    private fun editText(text: String, hint: String): EditText = EditText(this).apply {
        setText(text)
        this.hint = hint
        setTextColor(0xFFe6e6e6.toInt())
        setHintTextColor(0xFF777777.toInt())
        backgroundTintList = ColorStateList.valueOf(0xFF333742.toInt())
        setPadding(dp(8), dp(8), dp(8), dp(8))
    }

    private fun dp(n: Int): Int = (n * resources.displayMetrics.density).toInt()

    private fun spinnerAdapter() = ArrayAdapter<String>(this, android.R.layout.simple_spinner_dropdown_item).apply {
        setDropDownViewResource(android.R.layout.simple_spinner_dropdown_item)
    }
}