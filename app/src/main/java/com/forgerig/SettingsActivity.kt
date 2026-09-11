package com.forgerig

import android.os.Bundle
import android.view.Gravity
import android.view.ViewGroup
import android.widget.ArrayAdapter
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.Spinner
import android.widget.TextView
import android.widget.Toast
import androidx.appcompat.app.AppCompatActivity

class SettingsActivity : AppCompatActivity() {

    private val providerOptions = listOf(
        "openai - OpenAI (gpt-4o-mini)" to "openai",
        "openrouter - OpenRouter (free llama :free)" to "openrouter",
        "nvidia - NVIDIA NIM (free credits)" to "nvidia",
        "groq - Groq (free tier)" to "groq",
        "deepseek - DeepSeek (cheap)" to "deepseek",
        "mistral - Mistral (incl. leastral-1-5)" to "mistral",
        "gemini - Google Gemini (free tier)" to "gemini",
        "ollama - Local LLM" to "ollama",
        "custom - any OpenAI-compatible endpoint" to "custom",
    )

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val current = SettingsStore.load(this)

        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(20), dp(20), dp(20), dp(20))
        }

        fun label(text: String): TextView = TextView(this).apply {
            this.text = text
            setPadding(0, dp(14), 0, dp(4))
            textSize = 14f
        }

        root.addView(label("Provider"))
        val spinner = Spinner(this).apply {
            adapter = ArrayAdapter(
                this@SettingsActivity,
                android.R.layout.simple_spinner_dropdown_item,
                providerOptions.map { it.first },
            )
            val idx = providerOptions.indexOfFirst { it.second == current.provider }
            setSelection(if (idx >= 0) idx else 0)
        }
        root.addView(spinner)

        root.addView(label("Model (leave blank for provider default)"))
        val modelEdit = EditText(this).apply {
            setText(current.model)
            hint = "e.g. leastral-1-5 or llama-3.3-70b-versatile"
        }
        root.addView(modelEdit)

        root.addView(label("Evaluation model (blank = same as chat)"))
        val evalEdit = EditText(this).apply {
            setText(current.evalModel)
            hint = "cheap model for background memory evaluation"
        }
        root.addView(evalEdit)

        root.addView(label("Base URL (blank = provider default; required for custom)"))
        val urlEdit = EditText(this).apply {
            setText(current.baseUrl)
            hint = "https://host/api (OpenAI-compatible)"
        }
        root.addView(urlEdit)

        root.addView(label("API key"))
        val keyEdit = EditText(this).apply {
            setText(current.apiKey)
            hint = "stored encrypted on this device"
        }
        root.addView(keyEdit)

        root.addView(Button(this).apply {
            text = "Save"
            setOnClickListener {
                SettingsStore.save(
                    this@SettingsActivity,
                    SettingsStore.Settings(
                        provider = providerOptions[spinner.selectedItemPosition].second,
                        model = modelEdit.text.toString(),
                        evalModel = evalEdit.text.toString(),
                        baseUrl = urlEdit.text.toString(),
                        apiKey = keyEdit.text.toString(),
                    ),
                )
                Toast.makeText(this@SettingsActivity, "Saved. Restart the container to apply.", Toast.LENGTH_LONG).show()
                finish()
            }
        }, LinearLayout.LayoutParams(
            ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT
        ).apply { topMargin = dp(20) })

        root.addView(TextView(this).apply {
            text = "The API key is encrypted with the Android keystore and only injected into the container at launch — it never reaches the model through chat."
            setPadding(0, dp(14), 0, 0)
            textSize = 12f
            gravity = Gravity.CENTER
        })

        setContentView(ScrollView(this).apply { addView(root) })
    }

    private fun dp(v: Int): Int = (v * resources.displayMetrics.density).toInt()
}