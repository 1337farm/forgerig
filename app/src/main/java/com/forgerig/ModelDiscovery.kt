package com.forgerig

/**
 * Provider model-list discovery without Android UI dependencies.
 *
 * Curated catalogs remain the offline fallback. These helpers make the live
 * `/v1/models` style queries and merges deterministic enough for unit tests.
 */
object ModelDiscovery {
    data class ModelQuery(val url: String, val auth: String?)

    data class DiscoveredModel(val id: String, val free: Boolean)

    data class ProviderCounts(val code: String, val free: Int, val total: Int) {
        fun label(): String = "$free free / $total models"
    }

    fun buildQuery(provider: String, base: String, key: String): ModelQuery? {
        if (provider == "custom" && base.isEmpty()) return null
        val normalizedBase = base.trimEnd('/')
        return when (provider) {
            "gemini" -> ModelQuery("$normalizedBase/v1beta/models?key=$key", null)
            "ollama" -> ModelQuery("$normalizedBase/api/tags", null)
            else -> ModelQuery("$normalizedBase/v1/models", key.ifEmpty { null })
        }
    }

    fun discoverModels(provider: String, body: String): List<DiscoveredModel> {
        val ids = parseModelIds(provider, body)
        return ids.map { DiscoveredModel(it, inferFree(provider, it)) }
    }

    /**
     * Provider-aware free-model classification.
     *
     * The `/v1/models` payloads do not expose pricing/entitlement consistently:
     * - NVIDIA returns a browsable catalog where “callable with credits” does
     *   not mean “free”.
     * - OpenRouter marks genuinely free endpoints with `:free`; every other
     *   vendor-prefixed ID is billable even when it has a free-looking name.
     * - Gemini/Ollama/local catalog entries are covered by their free tiers or
     *   local execution, so names alone cannot disqualify them here.
     */
    fun inferFree(provider: String, modelId: String): Boolean {
        if (modelId.endsWith(":free")) return true
        // OpenRouter's `openrouter/auto` router can land on free endpoints
        // (and honors the account's free routing); every other vendor ID is
        // billable unless it carries the explicit `:free` suffix.
        if (provider == "openrouter" && modelId == "openrouter/auto") return true
        return when (provider) {
            "openrouter" -> false
            "nvidia" -> modelId == "nvidia/llama-3.1-nemotron-70b-instruct"
            else -> false
        }
    }

    fun providerCounts(code: String, options: List<Pair<String, Boolean>>): ProviderCounts {
        val unique = options.distinctBy { it.first }
        return ProviderCounts(code, unique.count { it.second }, unique.size)
    }

    fun parseModelIds(provider: String, body: String): List<String> {
        val ids = mutableListOf<String>()
        when (provider) {
            "ollama", "gemini" -> {
                val modelsStart = body.indexOf("\"models\"")
                val modelsEnd = body.lastIndexOf(']')
                if (modelsStart < 0 || modelsEnd <= modelsStart) return emptyList()
                val modelsBody = body.substring(modelsStart, modelsEnd + 1)
                val fieldPattern = Regex(""""name"\s*:\s*"((?:[^"\\]|\\.)*)"""")
                for (match in fieldPattern.findAll(modelsBody)) {
                    var name = unescapeJsonString(match.groupValues[1])
                    if (provider == "gemini") name = name.removePrefix("models/")
                    if (name.isNotEmpty() && !ids.contains(name)) ids.add(name)
                }
            }
            else -> {
                val fieldPattern = Regex(""""id"\s*:\s*"((?:[^"\\]|\\.)*)"""")
                for (match in fieldPattern.findAll(body)) {
                    val name = unescapeJsonString(match.groupValues[1])
                    if (name.isNotEmpty() && !ids.contains(name)) ids.add(name)
                }
            }
        }
        return ids
    }

    private fun unescapeJsonString(raw: String): String {
        val out = StringBuilder(raw.length)
        var i = 0
        while (i < raw.length) {
            val c = raw[i]
            if (c != '\\' || i + 1 >= raw.length) {
                out.append(c)
                i += 1
                continue
            }
            when (val next = raw[i + 1]) {
                '"', '\\', '/' -> {
                    out.append(next)
                    i += 2
                }
                'b' -> {
                    out.append('\b')
                    i += 2
                }
                'f' -> {
                    out.append('\u000C')
                    i += 2
                }
                'n' -> {
                    out.append('\n')
                    i += 2
                }
                'r' -> {
                    out.append('\r')
                    i += 2
                }
                't' -> {
                    out.append('\t')
                    i += 2
                }
                'u' -> {
                    if (i + 5 < raw.length) {
                        val hex = raw.substring(i + 2, i + 6)
                        val code = hex.toIntOrNull(16)
                        if (code != null) {
                            out.append(code.toChar())
                            i += 6
                            continue
                        }
                    }
                    out.append(next)
                    i += 2
                }
                else -> {
                    out.append(next)
                    i += 2
                }
            }
        }
        return out.toString()
    }

    fun mergeDiscovered(
        curated: List<String>,
        discovered: List<String>,
    ): List<String> {
        val merged = curated.toMutableList()
        for (id in discovered.sorted()) {
            if (!merged.contains(id)) merged.add(id)
        }
        return merged
    }
}
