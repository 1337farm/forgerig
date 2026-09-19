package com.forgerig

import android.content.Context
import android.content.SharedPreferences
import android.util.Log
import androidx.security.crypto.EncryptedSharedPreferences
import androidx.security.crypto.MasterKeys

/**
 * Persists provider/model/key options. The API key is written through
 * EncryptedSharedPreferences (Android KeyStore-backed AES-GCM), so it never
 * sits on disk in plaintext. Fail-closed: if the keystore is unavailable the
 * save is refused and load returns empty — no plaintext fallback, so keys can
 * never leak into an unencrypted prefs file that ingest would then carry into
 * model context.
 */
object SettingsStore {
    private const val TAG = "SettingsStore"
    private const val FILE = "forgerig_settings"

    // Scope constants for type-safe access
    const val SCOPE_GITHUB = "github"
    const val SCOPE_HF = "hf"

    private fun prefs(ctx: Context): SharedPreferences {
        // Throws on failure: callers fail closed (see load/save). No plaintext
        // fallback — a *_plain file would be readable by ingest-style scans
        // and backup agents.
        val masterKeyAlias = MasterKeys.getOrCreate(MasterKeys.AES256_GCM_SPEC)
        return EncryptedSharedPreferences.create(
            FILE,
            masterKeyAlias,
            ctx,
            EncryptedSharedPreferences.PrefKeyEncryptionScheme.AES256_SIV,
            EncryptedSharedPreferences.PrefValueEncryptionScheme.AES256_GCM,
        )
    }

    fun load(ctx: Context): Settings {
        val p = try {
            prefs(ctx)
        } catch (e: Exception) {
            Log.e(TAG, "Encrypted prefs unavailable; returning empty (fail-closed)", e)
            AssetExtractor.logShared(ctx, "ERROR: Encrypted prefs unavailable; keys withheld (fail-closed) | $e")
            return Settings()
        }
        return Settings(
            provider = p.getString("provider", "") ?: "",
            model = p.getString("model", "") ?: "",
            evalModel = p.getString("evalModel", "") ?: "",
            baseUrl = p.getString("baseUrl", "") ?: "",
            apiKey = p.getString("apiKey", "") ?: "",
            maxTokens = p.getString("maxTokens", "") ?: "",
        )
    }

    fun save(ctx: Context, s: Settings) {
        try {
            prefs(ctx).edit()
                .putString("provider", s.provider.trim())
                .putString("model", s.model.trim())
                .putString("evalModel", s.evalModel.trim())
                .putString("baseUrl", s.baseUrl.trim())
                .putString("apiKey", s.apiKey.trim())
                .putString("maxTokens", s.maxTokens.trim())
                // commit() (not apply()): the settings are read back synchronously by
                // ContainerService the moment we trigger a container restart, so the
                // new values must already be flushed to disk.
                .commit()
        } catch (e: Exception) {
            Log.e(TAG, "Encrypted prefs unavailable; save refused (fail-closed)", e)
            AssetExtractor.logShared(ctx, "ERROR: Settings save refused; keystore unavailable (fail-closed) | $e")
            throw IllegalStateException("Secure storage unavailable; key not saved", e)
        }
    }

    /** LLM scope for a provider. */
    fun llmScope(provider: String): String = "llm:$provider"

    /** Generate a deterministic scope for a custom OpenAI-compatible endpoint. */
    fun customScope(baseUrl: String): String {
        val hash = java.security.MessageDigest.getInstance("SHA-256")
            .digest(baseUrl.trim().toByteArray())
            .joinToString("") { "%02x".format(it) }
            .substring(0, 16)
        return "custom:$hash"
    }

    /** Get a key for a specific scope (e.g., "llm:openai", "github", "hf", "custom:sha256..."). */
    fun getKey(ctx: Context, scope: String): String {
        val p = prefs(ctx)
        return p.getString("key_$scope", "") ?: ""
    }

    /** Set a key for a specific scope. */
    fun setKey(ctx: Context, scope: String, key: String) {
        try {
            prefs(ctx).edit()
                .putString("key_$scope", key.trim())
                .commit()
        } catch (e: Exception) {
            Log.e(TAG, "Failed to save key for scope $scope", e)
        }
    }

    /** Delete a key for a scope. */
    fun deleteKey(ctx: Context, scope: String) {
        try {
            prefs(ctx).edit()
                .remove("key_$scope")
                .commit()
        } catch (e: Exception) {
            Log.e(TAG, "Failed to delete key for scope $scope", e)
        }
    }
}

data class Settings(
    val provider: String = "",
    val model: String = "",
    val evalModel: String = "",
    val baseUrl: String = "",
    val apiKey: String = "",
    val maxTokens: String = "",
)