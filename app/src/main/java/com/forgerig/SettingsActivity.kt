package com.forgerig

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.res.ColorStateList
import android.os.Build
import android.os.Bundle
import android.view.Gravity
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
        "mistral" to "https://api.mistral.ai/v1",
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

        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(20), dp(20), dp(20), dp(20))
            setBackgroundColor(0xFF151221.toInt())
        }
        root.addView(TextView(this).apply {
            text = "ForgeRig Settings"
            textSize = 20f
            setTextColor(0xFFd946ef.toInt())
            setPadding(0, 0, 0, dp(4))
        })

        fun spinnerAdapter(): ArrayAdapter<String> =
            object : ArrayAdapter<String>(this, android.R.layout.simple_spinner_item, mutableListOf()) {
                override fun getView(position: Int, convertView: View?, parent: ViewGroup): View =
                    (super.getView(position, convertView, parent) as TextView).apply {
                        textSize = 14f
                        setTextColor(0xFFe6e6e6.toInt())
                    }

                override fun getDropDownView(position: Int, convertView: View?, parent: ViewGroup): View =
                    (super.getDropDownView(position, convertView, parent) as TextView).apply {
                        textSize = 14f
                        setSingleLine(false)
                        setTextColor(0xFFe6e6e6.toInt())
                    }
            }

        fun label(text: String): TextView = TextView(this).apply {
            this.text = text
            setPadding(0, dp(14), 0, dp(4))
            textSize = 14f
            setTextColor(0xFFe6e6e6.toInt())
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

        fun editText(text: String, hint: String): EditText = EditText(this).apply {
            setText(text)
            this.hint = hint
            setTextColor(0xFFe6e6e6.toInt())
            setHintTextColor(0xFF777777.toInt())
            backgroundTintList = ColorStateList.valueOf(0xFF333742.toInt())
            setPadding(dp(8), dp(8), dp(8), dp(8))
        }

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
            override fun onTextChanged(s: CharSequence?, start: Int, before: Int, count: Int) = refreshModels()
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
            inputType = android.text.InputType.TYPE_CLASS_NUMBER
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

        fun fetchModels() {
            val provider = providerOptions[spinner.selectedItemPosition].second
            if (provider == "nvidia") {
                Toast.makeText(this, "NVIDIA has no model-list endpoint; curated list kept.", Toast.LENGTH_SHORT).show()
                return
            }
            val base = urlEdit.text.toString().trim().ifEmpty { defaultBaseUrls[provider] ?: "" }
            if (base.isEmpty()) {
                Toast.makeText(this, "Set a Base URL for the custom provider first.", Toast.LENGTH_SHORT).show()
                return
            }
            val key = keyEdit.text.toString().trim()
            Toast.makeText(this, "Refreshing model list…", Toast.LENGTH_SHORT).show()
            Thread {
                try {
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
                        runOnUiThread {
                            if (ids.isEmpty()) {
                                Toast.makeText(this, "No models returned.", Toast.LENGTH_SHORT).show()
                            } else {
                                val known = extraModels.getOrPut(provider) { mutableListOf() }
                                var added = 0
                                for (id in ids.sorted()) {
                                    if ((modelCatalog[provider] ?: emptyList()).none { it.name == id } && !known.contains(id)) {
                                        known.add(id)
                                        added++
                                    }
                                }
                                refreshModels()
                                Toast.makeText(this, "Added $added model(s), ${ids.size} total.", Toast.LENGTH_SHORT).show()
                            }
                        }
                    } finally {
                        conn.disconnect()
                    }
                } catch (e: Exception) {
                    runOnUiThread {
                        Toast.makeText(this, "Refresh failed: ${e.message}", Toast.LENGTH_LONG).show()
                    }
                }
            }.start()
        }
        root.addView(Button(this).apply {
            text = "Refresh models from provider"
            setBackgroundColor(0xFF3a3348.toInt())
            setTextColor(0xFFe6e6e6.toInt())
            setOnClickListener { fetchModels() }
        })
        refreshModels()

        root.addView(Button(this).apply {
            text = "Save"
            setBackgroundColor(0xFF9333ea.toInt())
            setTextColor(0xFFFFFFFF.toInt())
            setOnClickListener {
                SettingsStore.save(
                    this@SettingsActivity,
                    SettingsStore.Settings(
                        provider = providerOptions[spinner.selectedItemPosition].second,
                        model = modelEdit.text.toString(),
                        evalModel = evalEdit.text.toString(),
                        baseUrl = urlEdit.text.toString(),
                        apiKey = keyEdit.text.toString(),
                        maxTokens = maxTokensEdit.text.toString(),
                    ),
                )
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
            gravity = Gravity.CENTER
            setTextColor(0xFF777777.toInt())
        })

        setContentView(ScrollView(this).apply {
            setBackgroundColor(0xFF151221.toInt())
            addView(root)
        })
    }

    override fun onDestroy() {
        try {
            unregisterReceiver(finishReceiver)
        } catch (e: Exception) {
        }
        super.onDestroy()
    }

    private fun dp(v: Int): Int = (v * resources.displayMetrics.density).toInt()
}