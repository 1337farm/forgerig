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
        return ModelQuery("$normalizedBase/v1/models", key.ifEmpty { null })
    }

    /**
     * Per-provider key resolution for refresh-all: a provider's own saved
     * key wins; the typed field is only a fallback. This keeps one
     * provider's key from causing phantom 401s on every other provider.
     */
    fun resolveProviderKey(scopedKey: String, typedKey: String): String =
        scopedKey.ifBlank { typedKey }

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
        return modelId.endsWith(":free") || modelId.startsWith("nvidia/")
    }

    fun providerCounts(code: String, options: List<Pair<String, Boolean>>): ProviderCounts {
        val unique = options.distinctBy { it.first }
        return ProviderCounts(code, unique.count { it.second }, unique.size)
    }

    fun parseModelIds(provider: String, body: String): List<String> {
        val ids = mutableListOf<String>()
        val fieldPattern = Regex(""""id"\s*:\s*"((?:[^"\\]|\\.)*)"""")
        for (match in fieldPattern.findAll(body)) {
            val name = unescapeJsonString(match.groupValues[1])
            if (name.isNotEmpty() && !ids.contains(name)) ids.add(name)
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

    /**
     * Reconcile the persisted model list against a fresh fetch — pure function
     * of its inputs: no I/O, no Android, fully headless-testable.
     *
     * Membership AND flags both come from the live list. Entries that vanished
     * upstream are dropped and flags are re-taken from discovery, so a model
     * that was removed (or un-freed) can never linger with stale metadata and
     * 404 at chat time. Curated entries are excluded: they live in the
     * catalog, not in the persisted extras.
     *
     * Returns the fresh extras list plus how many ids are new since `known`.
     *
     * An empty discovery keeps `known` untouched: wiping the list on an empty
     * response would strand the user with no selectable model at all, and an
     * empty catalog response says nothing reliable about what still serves.
     */
    fun reconcileModels(
        curated: List<String>,
        known: List<DiscoveredModel>,
        discovered: List<DiscoveredModel>,
    ): Pair<List<DiscoveredModel>, Int> {
        if (discovered.isEmpty()) return known to 0
        val fresh = discovered.sortedBy { it.id }
            .filter { d -> curated.none { it == d.id } }
        val had = known.map { it.id }.toSet()
        return fresh to fresh.count { it.id !in had }
    }
}
