package com.forgerig

import android.content.Context
import android.content.SharedPreferences
import android.util.Log
import androidx.security.crypto.EncryptedSharedPreferences
import androidx.security.crypto.MasterKeys

/**
 * Persists provider/model/key options. The API key is written through
 * EncryptedSharedPreferences (Android KeyStore-backed AES-GCM), so it never
 * sits on disk in plaintext. Falls back to plain prefs only if the keystore is
 * unavailable on the device.
 */
object SettingsStore {
    private const val TAG = "SettingsStore"
    private const val FILE = "forgerig_settings"

    data class Settings(
        val provider: String = "",
        val model: String = "",
        val evalModel: String = "",
        val baseUrl: String = "",
        val apiKey: String = "",
    )

    private fun prefs(ctx: Context): SharedPreferences {
        return try {
            val masterKeyAlias = MasterKeys.getOrCreate(MasterKeys.AES256_GCM_SPEC)
            EncryptedSharedPreferences.create(
                FILE,
                masterKeyAlias,
                ctx,
                EncryptedSharedPreferences.PrefKeyEncryptionScheme.AES256_SIV,
                EncryptedSharedPreferences.PrefValueEncryptionScheme.AES256_GCM,
            )
        } catch (e: Exception) {
            Log.w(TAG, "Encrypted prefs unavailable; falling back to plaintext", e)
            AssetExtractor.logShared(ctx, "WARNING: Encrypted prefs unavailable; falling back to plaintext | $e")
            ctx.getSharedPreferences("${FILE}_plain", Context.MODE_PRIVATE)
        }
    }

    fun load(ctx: Context): Settings {
        val p = prefs(ctx)
        return Settings(
            provider = p.getString("provider", "") ?: "",
            model = p.getString("model", "") ?: "",
            evalModel = p.getString("evalModel", "") ?: "",
            baseUrl = p.getString("baseUrl", "") ?: "",
            apiKey = p.getString("apiKey", "") ?: "",
        )
    }

    fun save(ctx: Context, s: Settings) {
        prefs(ctx).edit()
            .putString("provider", s.provider.trim())
            .putString("model", s.model.trim())
            .putString("evalModel", s.evalModel.trim())
            .putString("baseUrl", s.baseUrl.trim())
            .putString("apiKey", s.apiKey.trim())
            .apply()
    }
}