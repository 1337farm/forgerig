package com.forgerig

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class ModelDiscoveryTest {

    @Test
    fun nvidiaUsesStandardModelListEndpointWithoutAuth() {
        val query = ModelDiscovery.buildQuery(
            "nvidia",
            "https://integrate.api.nvidia.com",
            "",
        )
        assertEquals("https://integrate.api.nvidia.com/v1/models", query?.url)
        assertNull(query?.auth)
    }

    @Test
    fun mistralBaseDoesNotDuplicateApiVersion() {
        val query = ModelDiscovery.buildQuery(
            "mistral",
            "https://api.mistral.ai",
            "test-key",
        )
        assertEquals("https://api.mistral.ai/v1/models", query?.url)
        assertEquals("test-key", query?.auth)
    }

    @Test
    fun customWithoutBaseIsSkipped() {
        assertNull(ModelDiscovery.buildQuery("custom", "", "test-key"))
    }

    @Test
    fun parsesOpenAiAndNvidiaModelIds() {
        val ids = ModelDiscovery.parseModelIds(
            "nvidia",
            """{"data":[{"id":"nvidia/llama-3.1-nemotron-70b-instruct"},{"id":""}]}""",
        )
        assertEquals(listOf("nvidia/llama-3.1-nemotron-70b-instruct"), ids)
    }

    @Test
    fun parsesGeminiNamesAndStripsPrefix() {
        val ids = ModelDiscovery.parseModelIds(
            "gemini",
            """{"models":[{"name":"models/gemini-2.5-flash"}]}""",
        )
        assertEquals(listOf("gemini-2.5-flash"), ids)
    }

    @Test
    fun mergesCuratedAndDiscoveredWithoutReplacingFallback() {
        val merged = ModelDiscovery.mergeDiscovered(
            listOf("nvidia/llama-3.1-nemotron-70b-instruct"),
            listOf("nvidia/llama-3.1-nemotron-70b-instruct", "nvidia/new-model"),
        )
        assertEquals(
            listOf(
                "nvidia/llama-3.1-nemotron-70b-instruct",
                "nvidia/new-model",
            ),
            merged,
        )
        assertTrue(merged.contains("nvidia/llama-3.1-nemotron-70b-instruct"))
    }
}
