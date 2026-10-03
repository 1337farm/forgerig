package com.forgerig

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
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

    @Test
    fun nvidiaCatalogIdsAreNotAllFree() {
        assertTrue(ModelDiscovery.inferFree("nvidia", "nvidia/llama-3.1-nemotron-70b-instruct"))
        assertFalse(ModelDiscovery.inferFree("nvidia", "meta/llama-3.1-405b-instruct"))
        assertFalse(ModelDiscovery.inferFree("nvidia", "mistralai/mixtral-8x22b-instruct-v0.1"))
        assertFalse(ModelDiscovery.inferFree("nvidia", "deepseek-ai/deepseek-r1"))
        assertTrue(ModelDiscovery.inferFree("nvidia", "nvidia/nemotron-4-340b-instruct"))
    }

    @Test
    fun discoverModelsCarriesProviderFreeFlags() {
        val models = ModelDiscovery.discoverModels(
            "nvidia",
            """{"data":[{"id":"nvidia/llama-3.1-nemotron-70b-instruct"},{"id":"nvidia/nemotron-4-340b-instruct"}]}""",
        )
        assertEquals(2, models.size)
        assertTrue(models.first { it.id == "nvidia/llama-3.1-nemotron-70b-instruct" }.free)
        assertTrue(models.first { it.id == "nvidia/nemotron-4-340b-instruct" }.free)
    }

    @Test
    fun providerCountsLabelShowsFreeOverTotal() {
        val counts = ModelDiscovery.providerCounts(
            "nvidia",
            listOf(
                "nvidia/llama-3.1-nemotron-70b-instruct" to true,
                "nvidia/nemotron-4-340b-instruct" to false,
                "nvidia/llama-3.1-nemotron-70b-instruct" to true,
            ),
        )
        assertEquals(1, counts.free)
        assertEquals(2, counts.total)
        assertEquals("1 free / 2 models", counts.label())
    }

    @Test
    fun scopedKeyWinsOverTypedKey() {
        assertEquals("scoped", ModelDiscovery.resolveProviderKey("scoped", "typed"))
        assertEquals("typed", ModelDiscovery.resolveProviderKey("", "typed"))
        assertEquals("", ModelDiscovery.resolveProviderKey("", ""))
        assertEquals("scoped", ModelDiscovery.resolveProviderKey("scoped", ""))
    }
}
