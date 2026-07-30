// Offline shell for the chat app. The server stamps __REV__ with the build's
// asset digest, so the cache name changes exactly when a published version
// serves different bytes - within a version everything here is immutable.
"use strict";

var REV = "__REV__";
var CACHE = "eyesoff-shell-" + REV;

// The registration scope is the app ROOT this worker was registered under: the
// custom-domain root, or the platform's /x/<id>/https/ prefix. Every cached
// URL is resolved against it; an absolute path would escape the prefix.
var SCOPE = new URL(self.registration.scope);

// Shell = every static byte the app serves. "" is the page itself; /c/<id>
// chat paths serve those same bytes, so the root entry answers for all of
// them. API routes (chat, models, attestation, search) are deliberately
// absent: those must always hit the enclave, and the streaming POSTs are
// never intercepted at all.
var SHELL = [
  "",
  "emoji.woff2",
  "favicon.svg",
  "favicon.ico",
  "apple-touch-icon.png",
  "manifest.webmanifest",
  "icon-192.png",
  "icon-512.png",
  "icon-maskable-512.png",
];

function shellURL(p) {
  return new URL(p, SCOPE).href;
}

self.addEventListener("install", function (e) {
  e.waitUntil(
    caches
      .open(CACHE)
      .then(function (c) {
        return Promise.all(
          SHELL.map(function (p) {
            // cache: "reload" - the static assets are served with a year of
            // immutable, so filling the worker cache through the HTTP cache
            // could re-pin bytes from the PREVIOUS version at a stable
            // custom domain. A new worker exists to fetch new bytes.
            return c.add(new Request(shellURL(p), { cache: "reload" }));
          })
        );
      })
      .then(function () {
        return self.skipWaiting();
      })
  );
});

self.addEventListener("activate", function (e) {
  e.waitUntil(
    caches
      .keys()
      .then(function (keys) {
        return Promise.all(
          keys
            .filter(function (k) {
              return k !== CACHE;
            })
            .map(function (k) {
              return caches.delete(k);
            })
        );
      })
      .then(function () {
        return self.clients.claim();
      })
  );
});

// Path relative to the scope, or null when the request lives outside it.
function rel(url) {
  if (url.origin !== SCOPE.origin) return null;
  if (!url.pathname.startsWith(SCOPE.pathname)) return null;
  return url.pathname.slice(SCOPE.pathname.length);
}

// Mirrors is_chat_path in lib.rs: c, c/, c/<id> - one path segment, so a
// deeper path never silently swallows a future route.
function isShellNav(r) {
  var t = r.replace(/\/+$/, "");
  return t === "" || t === "c" || /^c\/[^/]+$/.test(t);
}

self.addEventListener("fetch", function (e) {
  var req = e.request;
  if (req.method !== "GET") return; // chat/title/completions POSTs pass through
  var r = rel(new URL(req.url));
  if (r === null) return;

  if (req.mode === "navigate" && isShellNav(r)) {
    // Cache-first for an instant, offline-capable open; refresh the stored
    // copy in the background. A version change lands via the rev-keyed
    // cache swap, so serving the cached page here is never a downgrade the
    // next load would not fix.
    e.respondWith(
      (function () {
        var root = shellURL("");
        return caches.open(CACHE).then(function (c) {
          return c.match(root).then(function (hit) {
            var refresh = fetch(req)
              .then(function (res) {
                if (res && res.ok) c.put(root, res.clone());
                return res;
              })
              .catch(function () {
                return undefined;
              });
            e.waitUntil(refresh);
            return (
              hit ||
              refresh.then(function (res) {
                return res || Response.error();
              })
            );
          });
        });
      })()
    );
    return;
  }

  if (SHELL.indexOf(r) >= 0) {
    e.respondWith(
      caches.match(shellURL(r)).then(function (hit) {
        return hit || fetch(req);
      })
    );
  }
  // Everything else (models, ping, attestation, search, warmup, v1/*) goes
  // straight to the network untouched.
});
