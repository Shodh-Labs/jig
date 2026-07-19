"use strict";

// A tiny HTTP(S) GET-to-Buffer helper built on Node's `http`/`https` builtins.
// Zero runtime dependencies by design.
//
// * Follows redirects (GitHub Releases 302-redirect asset URLs to a signed
//   objects CDN host), with a bounded hop count.
// * Supports both https:// (real releases) and http:// (the localhost download
//   simulation test and air-gapped mirrors), chosen per-URL.
//
// Proxy support: Node's http/https do NOT natively honor HTTPS_PROXY, and doing
// it correctly (HTTP CONNECT tunnelling + TLS-over-tunnel) is enough moving
// parts that a half-implementation would be worse than none. We therefore
// detect a configured proxy and warn clearly, pointing at the JIG_BINARY_PATH
// escape hatch, rather than silently ignoring it. See README "Behind a proxy".

const http = require("http");
const https = require("https");

const MAX_REDIRECTS = 5;

/** Emit a one-time warning if a proxy is configured but unsupported. */
function warnIfProxyConfigured() {
  const proxy =
    process.env.HTTPS_PROXY ||
    process.env.https_proxy ||
    process.env.HTTP_PROXY ||
    process.env.http_proxy;
  if (proxy) {
    console.warn(
      `@shodh/jig: a proxy is configured (${proxy}) but this installer connects ` +
        `directly and cannot use it. If the download fails, download the release ` +
        `archive manually and set JIG_BINARY_PATH to the extracted binary, or ` +
        `install via cargo. See the package README.`
    );
  }
}

/**
 * Download `url` into a Buffer, following redirects.
 * @param {string} url
 * @param {number} [redirectsLeft]
 * @returns {Promise<Buffer>}
 */
function downloadBuffer(url, redirectsLeft = MAX_REDIRECTS) {
  return new Promise((resolve, reject) => {
    const client = url.startsWith("https:") ? https : http;
    const req = client.get(url, { headers: { "user-agent": "shodh-jig-npm-installer" } }, (res) => {
      const { statusCode, headers } = res;

      // Redirect handling.
      if (statusCode >= 300 && statusCode < 400 && headers.location) {
        res.resume(); // drain
        if (redirectsLeft <= 0) {
          reject(new Error(`@shodh/jig: too many redirects fetching ${url}`));
          return;
        }
        const next = new URL(headers.location, url).toString();
        resolve(downloadBuffer(next, redirectsLeft - 1));
        return;
      }

      if (statusCode !== 200) {
        res.resume();
        reject(
          new Error(
            `@shodh/jig: download failed for ${url} — HTTP ${statusCode}. ` +
              `If the release does not exist yet, try again after it is published, ` +
              `or set JIG_BINARY_PATH to a local binary.`
          )
        );
        return;
      }

      const chunks = [];
      res.on("data", (c) => chunks.push(c));
      res.on("end", () => resolve(Buffer.concat(chunks)));
      res.on("error", reject);
    });
    req.on("error", reject);
    req.setTimeout(60000, () => {
      req.destroy(new Error(`@shodh/jig: download timed out after 60s for ${url}`));
    });
  });
}

module.exports = { downloadBuffer, warnIfProxyConfigured };
