package com.forgerig

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The daemon auth token is only handed to the daemon's own page. The bridge is
 * bound to the WebView, and `shouldOverrideUrlLoading` lets http/https
 * navigations load in it, so a foreign origin landing in the WebView could read
 * the token and then reach the daemon's `exec` RPC and the provider secrets
 * behind it. These tests pin the origin check that prevents that.
 */
class AuthTokenScopeTest {

    private val port = 41234
    private val daemon = "http://127.0.0.1:$port"

    @Test
    fun theDaemonOriginGetsTheToken() {
        assertEquals(MainActivity.authToken, authTokenForPage(daemon, port))
        assertEquals(MainActivity.authToken, authTokenForPage("$daemon/", port))
        // Query and fragment trail the path, not the origin.
        assertEquals(MainActivity.authToken, authTokenForPage("$daemon/?x=1", port))
        assertEquals(MainActivity.authToken, authTokenForPage("$daemon/#top", port))
        assertEquals(MainActivity.authToken, authTokenForPage("$daemon/?a=1#top", port))
    }

    @Test
    fun foreignOriginsGetNothing() {
        // A remote page in the WebView: no token.
        assertNull(authTokenForPage("https://example.com/", port))
        assertNull(authTokenForPage("http://example.com/", port))
        // Same host, wrong port — a different daemon (or a stranger's).
        assertNull(authTokenForPage("http://127.0.0.1:1/", port))
        assertNull(authTokenForPage("http://127.0.0.1:412340/", port))
        // Same port, wrong host: loopback aliases and DNS names.
        assertNull(authTokenForPage("http://localhost:$port/", port))
        assertNull(authTokenForPage("http://127.0.0.2:$port/", port))
        assertNull(authTokenForPage("http://[::1]:$port/", port))
        // Prefix tricks: the origin must be exact, not a string prefix.
        assertNull(authTokenForPage("http://127.0.0.1:$port.evil.com/", port))
        assertNull(authTokenForPage("http://127.0.0.1:$port@evil.com/", port))
        assertNull(authTokenForPage("http://evil.com/?x=http://127.0.0.1:$port/", port))
        // A subpath is not our page, even on the right origin.
        assertNull(authTokenForPage("$daemon/evil", port))
        assertNull(authTokenForPage("$daemon/assets/x.js", port))
        // https and wss on the daemon port are not the page we serve.
        assertNull(authTokenForPage("https://127.0.0.1:$port/", port))
        // file:/data: pages.
        assertNull(authTokenForPage("file:///data/data/com.forgerig/index.html", port))
        assertNull(authTokenForPage("data:text/html,<h1>hi</h1>", port))
        assertNull(authTokenForPage(null, port))
        assertNull(authTokenForPage(daemon, 0))
    }

    /**
     * `shouldOverrideUrlLoading` used to pass every http/https navigation to the
     * WebView, on the assumption that `authTokenForPage` was a sufficient
     * backstop. It is not: the token is the only page-scoped bridge method.
     * Everything else resolves for whatever document is loaded, so a foreign
     * origin in this WebView could call the daemon directly. Navigation is the
     * real control, and it now uses the same predicate as the token.
     */
    @Test
    fun onlyTheDaemonOriginMayNavigateInTheWebView() {
        assertTrue(isDaemonOrigin(daemon, port))
        assertTrue(isDaemonOrigin("$daemon/", port))
        assertTrue(isDaemonOrigin("$daemon/?x=1", port))
        assertTrue(isDaemonOrigin("$daemon/#top", port))
        // Everything authTokenForPage rejects must also be refused navigation —
        // that symmetry is the property that matters.
        for (url in listOf(
            "https://example.com/", "http://example.com/", "http://127.0.0.1:1/",
            "http://127.0.0.1:412340/", "http://localhost:$port/", "http://[::1]:$port/",
            "http://127.0.0.1:$port.evil.com/", "http://127.0.0.1:$port@evil.com/",
            "http://evil.com/?x=$daemon/", "$daemon/evil", "$daemon/assets/x.js",
            "https://127.0.0.1:$port/", "file:///data/data/com.forgerig/index.html",
            "data:text/html,<h1>hi</h1>", "javascript:alert(1)", "about:blank"
        )) {
            assertFalse("must not navigate in-app: $url", isDaemonOrigin(url, port))
        }
        assertFalse(isDaemonOrigin(null, port))
        assertFalse(isDaemonOrigin(daemon, 0))
        // No daemon yet (port defaults before the socket binds) must not be a
        // free pass for everything.
        assertFalse(isDaemonOrigin(daemon, 3000))
    }
}
