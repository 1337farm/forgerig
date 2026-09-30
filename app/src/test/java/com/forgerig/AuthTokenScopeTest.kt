package com.forgerig

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
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
}
