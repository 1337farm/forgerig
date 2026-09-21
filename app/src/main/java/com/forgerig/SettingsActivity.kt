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

    private companion object {
        const val EXTRA_MODELS_PREFS = "forgerig_extra_models"
        const val PROVIDER_AUTO = "openrouter/auto"
        const val FREE_SUFFIX = "  (free)"
    }

    private val providerOptions = listOf(
        "openai - OpenAI (gpt-4o-mini)" to "openai",
        "openrouter - OpenRouter (free + auto)" to "openrouter",
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
            ModelOption(PROVIDER_AUTO, true),
            ModelOption("nvidia/nemotron-3.5-lightning:free", true),
            ModelOption("nvidia/nemotron-3-nano-omni-30b-a3b-reasoning:free", true),
            ModelOption("nvidia/nemotron-3-super-120b-a12b:free", true),
            ModelOption("nvidia/nemotron-3-ultra-550b-a55b:free", true),
        ),
        "nvidia" to listOf(
            ModelOption("nvidia/llama-3.1-nemotron-70b-instruct", true),
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

    // Live-fetched model IDs merged over the curated catalog (per provider)
    // and persisted so a restart does not lose provider discovery. Free flags
    // are persisted explicitly because `/v1/models` payloads do not expose
    // pricing/entitlement consistently.
    private val extraModels: MutableMap<String, MutableList<ModelOption>> = mutableMapOf()

    private fun loadExtraModels() {
        val stored = getSharedPreferences(EXTRA_MODELS_PREFS, MODE_PRIVATE)
            .getStringSet("extra_models", emptySet()) ?: emptySet()
        for (entry in stored.sorted()) {
            val parts = entry.split('\u0001')
            if (parts.size == 3) {
                val provider = parts[0]
                val free = parts[1] == "1"
                val model = parts[2]
                if (provider.isNotEmpty() && model.isNotEmpty()) {
                    extraModels.getOrPut(provider) { mutableListOf() }.addIfAbsent(ModelOption(model, free))
                }
            } else if (parts.size == 2) {
                // Legacy entries stored only `provider + model`; re-infer the
                // free flag instead of dropping previously discovered models.
                val provider = parts[0]
                val model = parts[1]
                if (provider.isNotEmpty() && model.isNotEmpty()) {
                    extraModels.getOrPut(provider) { mutableListOf() }
                        .addIfAbsent(ModelOption(model, ModelDiscovery.inferFree(provider, model)))
                }
            }
        }
    }

    private fun persistExtraModels() {
        val flattened = extraModels.flatMap { (provider, models) ->
            models.map { "$provider\u0001${if (it.free) "1" else "0"}\u0001${it.name}" }
        }.toSet()
        getSharedPreferences(EXTRA_MODELS_PREFS, MODE_PRIVATE)
            .edit()
            .putStringSet("extra_models", flattened)
            .apply()
    }

    private fun MutableList<ModelOption>.addIfAbsent(model: ModelOption): Boolean {
        if (none { it.name == model.name }) {
            add(model)
            return true
        }
        return false
    }

    private fun allOptions(provider: String): List<ModelOption> {
        val merged = (modelCatalog[provider] ?: emptyList()).toMutableList()
        for (extra in extraModels[provider] ?: emptyList()) {
            merged.addIfAbsent(extra)
        }
        return merged
    }

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
        loadExtraModels()

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
        val providerCountView = TextView(this).apply {
            setTextColor(0xFF999999.toInt())
            textSize = 12f
            setPadding(0, dp(2), 0, 0)
        }
        root.addView(providerCountView)

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
            val idx = names.indexOfFirst { it.removeSuffix(FREE_SUFFIX) == selected }
            if (idx >= 0) view.setSelection(idx)
        }
        fun providerLabel(code: String): String {
            val base = providerOptions.firstOrNull { it.second == code }?.first ?: code
            val counts = ModelDiscovery.providerCounts(
                code,
                allOptions(code).map { it.name to it.free },
            )
            return "$base — ${counts.label()}"
        }
        fun refreshProviderLabels() {
            @Suppress("UNCHECKED_CAST")
            val adapter = spinner.adapter as ArrayAdapter<String>
            val selected = providerOptions[spinner.selectedItemPosition].second
            adapter.clear()
            adapter.addAll(providerOptions.map { providerLabel(it.second) })
            adapter.notifyDataSetChanged()
            val idx = providerOptions.indexOfFirst { it.second == selected }
            if (idx >= 0 && spinner.selectedItemPosition != idx) spinner.setSelection(idx)
        }
        fun refreshModels() {
            if (refreshingModels) return
            refreshingModels = true
            try {
                refreshProviderLabels()
                val provider = providerOptions[spinner.selectedItemPosition].second
                val query = modelFilterEdit.text.toString().trim().lowercase()
                val known = allOptions(provider)
                val counts = ModelDiscovery.providerCounts(
                    provider,
                    known.map { it.name to it.free },
                )
                providerCountView.text = "$provider: ${counts.label()}"
                val names = known
                    .filter { (!freeOnlyBox.isChecked || it.free) && (query.isEmpty() || it.name.lowercase().contains(query)) }
                    .map { if (it.free) "${it.name}$FREE_SUFFIX" else it.name }
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
                modelEdit.setText(item.removeSuffix(FREE_SUFFIX))
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
        root.addView(TextView(this).apply {
            text = "The key is saved per provider — refresh-all queries each provider with its own saved key."
            setTextColor(0xFF999999.toInt())
            textSize = 12f
            setPadding(0, dp(2), 0, 0)
        })
        root.addView(label("Max output tokens (blank = provider default)"))
        val maxTokensEdit = editText(current.maxTokens, "e.g. 2000").apply {
            setInputType(android.text.InputType.TYPE_CLASS_NUMBER)
        }
        root.addView(maxTokensEdit)

        evalSpinner.onItemSelectedListener = object : android.widget.AdapterView.OnItemSelectedListener {
            override fun onItemSelected(parent: android.widget.AdapterView<*>?, view: android.view.View?, position: Int, id: Long) {
                if (refreshingModels) return
                val item = parent?.getItemAtPosition(position) as? String ?: return
                evalEdit.setText(item.removeSuffix(FREE_SUFFIX))
            }
            override fun onNothingSelected(parent: android.widget.AdapterView<*>?) {}
        }

        fun storedKeyFor(provider: String, base: String): String {
            return try {
                val scope = if (provider == "custom" && base.isNotBlank()) {
                    SettingsStore.customScope(base)
                } else {
                    SettingsStore.llmScope(provider)
                }
                SettingsStore.getKey(this@SettingsActivity, scope)
            } catch (e: Exception) {
                // Keystore unavailable: fail closed to blank (callers then
                // report auth/connection failures instead of crashing).
                ""
            }
        }

        fun keyFor(provider: String, base: String, typedKey: String): String =
            ModelDiscovery.resolveProviderKey(storedKeyFor(provider, base), typedKey)

        fun prefillKeyFor(provider: String) {
            if (keyEdit.text.toString().isBlank()) {
                val saved = storedKeyFor(
                    provider,
                    if (provider == "custom") urlEdit.text.toString().trim() else "",
                )
                if (saved.isNotBlank()) keyEdit.setText(saved)
            }
        }

        fun fetchModelsFor(provider: String, base: String, key: String): List<ModelDiscovery.DiscoveredModel> {
            val query = ModelDiscovery.buildQuery(provider, base, key)
                ?: return emptyList()
            val conn = java.net.URL(query.url).openConnection() as java.net.HttpURLConnection
            try {
                conn.connectTimeout = 15000
                conn.readTimeout = 15000
                if (query.auth != null) conn.setRequestProperty("Authorization", "Bearer ${query.auth}")
                if (conn.responseCode !in 200..299) throw java.io.IOException("HTTP ${conn.responseCode}")
                val body = conn.inputStream.bufferedReader().readText()
                return ModelDiscovery.discoverModels(provider, body)
            } finally {
                conn.disconnect()
            }
        }

        fun fetchModels() {
            Toast.makeText(this, "Refreshing all providers…", Toast.LENGTH_SHORT).show()
            Thread {
                try {
                    val typedKey = keyEdit.text.toString().trim()
                    val customBase = urlEdit.text.toString().trim()
                    var totalAdded = 0
                    var totalSeen = 0
                    val failures = mutableListOf<String>()
                    val skipped = mutableListOf<String>()
                    for ((_, code) in providerOptions) {
                        // Each provider is queried against its own default base
                        // (custom keeps the typed URL); one provider's outage
                        // must not abort the rest.
                        val base = if (code == "custom") customBase else defaultBaseUrls[code] ?: ""
                        if (base.isEmpty()) continue
                        if (code == "ollama" && base == defaultBaseUrls["ollama"]) {
                            // Local Ollama is optional and often not installed:
                            // keep the curated local fallback visible instead of
                            // reporting a connection failure every refresh.
                            skipped.add("ollama: local server not configured")
                            continue
                        }
                        // Per-provider keys: the single typed field belongs to
                        // the selected provider only. Other providers use their
                        // own saved key so one provider's key never causes
                        // phantom 401s elsewhere.
                        val key = keyFor(code, base, typedKey)
                        try {
                            val discovered = fetchModelsFor(code, base, key)
                            totalSeen += discovered.size
                            val known = extraModels.getOrPut(code) { mutableListOf() }
                            for (model in discovered.sortedBy { it.id }) {
                                if ((modelCatalog[code] ?: emptyList()).none { it.name == model.id }) {
                                    if (known.addIfAbsent(ModelOption(model.id, model.free))) totalAdded++
                                }
                            }
                            persistExtraModels()
                        } catch (e: Exception) {
                            failures.add("$code: ${e.message}")
                            AssetExtractor.logShared(this, "WARNING: model refresh failed for $code | $e")
                        }
                    }
                    runOnUiThread {
                        refreshModels()
                        val notes = (failures + skipped).joinToString("; ")
                        val tail = if (notes.isEmpty()) "" else " Notes: $notes"
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
        // Prefill the key field from the selected provider's own saved key
        // (legacy single key as fallback). Switching providers never
        // clobbers typed text — only a blank field is filled in.
        prefillKeyFor(providerOptions[spinner.selectedItemPosition].second)
        spinner.onItemSelectedListener = object : android.widget.AdapterView.OnItemSelectedListener {
            override fun onItemSelected(parent: android.widget.AdapterView<*>?, view: android.view.View?, position: Int, id: Long) {
                prefillKeyFor(providerOptions[position].second)
                refreshModels()
            }
            override fun onNothingSelected(parent: android.widget.AdapterView<*>?) {}
        }
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
                    val selectedProvider = providerOptions[spinner.selectedItemPosition].second
                    SettingsStore.save(
                        this@SettingsActivity,
                        Settings(
                            provider = selectedProvider,
                            model = modelEdit.text.toString(),
                            evalModel = evalEdit.text.toString(),
                            baseUrl = urlEdit.text.toString(),
                            apiKey = keyEdit.text.toString(),
                            maxTokens = maxTokensEdit.text.toString(),
                        ),
                    )
                    // Persist the typed key under the selected provider's own
                    // scope so refresh-all and the daemon use per-provider
                    // keys (legacy single-key field stays as fallback).
                    try {
                        val typedKey = keyEdit.text.toString().trim()
                        if (typedKey.isNotEmpty()) {
                            val typedBase = urlEdit.text.toString().trim()
                            val scope = if (selectedProvider == "custom" && typedBase.isNotEmpty()) {
                                SettingsStore.customScope(typedBase)
                            } else {
                                SettingsStore.llmScope(selectedProvider)
                            }
                            SettingsStore.setKey(this@SettingsActivity, scope, typedKey)
                        }
                    } catch (e: Exception) {
                        AssetExtractor.logShared(this@SettingsActivity, "WARNING: per-provider key save failed | $e")
                    }
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