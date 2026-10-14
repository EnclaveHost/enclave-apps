// user.js: lightweight Firefox ESR profile
// Put this file in your profile folder (find it in about:profiles), then restart Firefox.
// Firefox re-applies these on every launch, so delete a line here to undo it.

// ---------- Fewer processes (biggest RAM saving) ----------
user_pref("dom.ipc.processCount", 2);                 // web content processes (default 8)
user_pref("fission.autostart", false);                // turn off per-site process isolation (security tradeoff, see notes)
user_pref("dom.ipc.processPrelaunch.enabled", false); // don't keep a spare process warm

// ---------- Memory behavior ----------
user_pref("browser.tabs.unloadOnLowMemory", true);    // unload background tabs when RAM gets tight
user_pref("browser.sessionhistory.max_total_viewers", 0); // don't keep old pages cached in RAM for back/forward
user_pref("browser.sessionhistory.max_entries", 10);  // shorter back/forward history per tab
user_pref("browser.sessionstore.interval", 60000);    // save session every 60s instead of 15s (less disk I/O)
user_pref("accessibility.force_disabled", 1);         // don't load the accessibility engine (remove if you use a screen reader)

// ---------- Startup and new tab ----------
user_pref("browser.startup.homepage", "about:blank");
user_pref("browser.newtabpage.enabled", false);       // blank new tab instead of the Firefox Home page
user_pref("browser.shell.checkDefaultBrowser", false);
user_pref("browser.aboutwelcome.enabled", false);

// ---------- Built-in extras you don't need ----------
user_pref("extensions.pocket.enabled", false);
user_pref("browser.ml.enable", false);                // local AI/ML features
user_pref("browser.ml.chat.enabled", false);          // AI chatbot sidebar
user_pref("browser.discovery.enabled", false);        // add-on recommendations
user_pref("extensions.htmlaboutaddons.recommendations.enabled", false);

// ---------- Telemetry and studies (background work + network) ----------
user_pref("datareporting.healthreport.uploadEnabled", false);
user_pref("datareporting.policy.dataSubmissionEnabled", false);
user_pref("toolkit.telemetry.enabled", false);
user_pref("toolkit.telemetry.unified", false);
user_pref("toolkit.telemetry.archive.enabled", false);
user_pref("app.shield.optoutstudies.enabled", false);
user_pref("app.normandy.enabled", false);
user_pref("browser.crashReports.unsubmittedCheck.autoSubmit2", false);

// ---------- Network: don't fetch things you didn't ask for ----------
user_pref("network.prefetch-next", false);
user_pref("network.dns.disablePrefetch", true);
user_pref("network.predictor.enabled", false);
user_pref("browser.urlbar.speculativeConnect.enabled", false);
user_pref("browser.places.speculativeConnect.enabled", false);

// ---------- Media ----------
user_pref("media.autoplay.default", 5);               // block all autoplay (audio and video)

// ---------- RISC Box ----------
// The machine's disk is 3 GiB with little free space; Firefox's automatic cache size could fill it.
user_pref("browser.cache.disk.smart_size.enabled", false);
user_pref("browser.cache.disk.capacity", 65536);      // 64 MB disk cache
// riscv64 JIT build (2026-10-09): WebAssembly's optimizing tier crashes on tier-up; keep wasm on its baseline JIT
user_pref("javascript.options.wasm_optimizingjit", false);
// RISC Box (2026-10-11): one HTTPS handshake costs ~24 s on this emulated CPU, past Firefox's 30 s handshake
// timeout once anything else is busy, so every https:// page failed. Wait for slow handshakes instead.
user_pref("network.http.tls-handshake-timeout", 300); // default 30
user_pref("network.http.connection-timeout", 300);    // default 90
user_pref("network.http.response.timeout", 600);      // default 300
// Background HTTPS Firefox makes on its own: each connection costs ~24 s of CPU that page loads wait behind.
user_pref("network.connectivity-service.enabled", false);
user_pref("network.captive-portal-service.enabled", false);
user_pref("dom.push.connection.enabled", false);
// Safe Browsing list downloads (security tradeoff: no phishing/malware warnings; Steven chose this 2026-10-11)
user_pref("browser.safebrowsing.malware.enabled", false);
user_pref("browser.safebrowsing.phishing.enabled", false);
user_pref("browser.safebrowsing.downloads.enabled", false);
user_pref("browser.safebrowsing.downloads.remote.enabled", false);
user_pref("browser.safebrowsing.blockedURIs.enabled", false);
// RISC Box (2026-10-11): every animation frame is painted in software on a ~50 MIPS emulated core, so an animating
// spinner or panel kept the browser at ~80% CPU while idle and starved everything else on the machine.
user_pref("layout.frame_rate", 10);                   // repaint at most 10 times a second (default: the display's 60)
user_pref("ui.prefersReducedMotion", 1);              // static loading spinner, no tab/panel animations
user_pref("general.smoothScroll", false);             // one paint per scroll step instead of an animation
user_pref("image.animation_mode", "once");            // animated images play once, then stop
user_pref("browser.tabs.hoverPreview.enabled", false);
// "Give your browsing a boost" add-on and feature recommendation popups
user_pref("browser.newtabpage.activity-stream.asrouter.userprefs.cfr.addons", false);
user_pref("browser.newtabpage.activity-stream.asrouter.userprefs.cfr.features", false);
// RISC Box (2026-10-11): background update/sync work Firefox repeats on every boot of a reset image
user_pref("security.remote_settings.crlite_filters.enabled", false);
user_pref("security.pki.crlite_mode", 0);
user_pref("extensions.update.enabled", false);
user_pref("extensions.update.autoUpdateDefault", false);
user_pref("extensions.getAddons.cache.enabled", false);
user_pref("extensions.systemAddon.update.enabled", false);
user_pref("browser.region.update.enabled", false);
user_pref("browser.search.update", false);
user_pref("network.process.enabled", false);
user_pref("browser.tabs.remote.separatePrivilegedContentProcess", false);
user_pref("browser.startup.homepage_override.mstone", "ignore");
user_pref("browser.sessionstore.resume_from_crash", false);
// RISC Box (2026-10-13): time to first page.
// TLS: without AES hardware, AES-GCM decryption dominated page loads; drop the AES-128-GCM suites so ChaCha20-Poly1305
// is offered first (servers that honour the client's order pick it; AES-256-GCM stays as the fallback).
user_pref("security.tls13.aes_128_gcm_sha256", false);
user_pref("security.ssl3.ecdhe_ecdsa_aes_128_gcm_sha256", false);
user_pref("security.ssl3.ecdhe_rsa_aes_128_gcm_sha256", false);
// a web content process kept ready: opening a site no longer starts one (~20 s here)
user_pref("dom.ipc.processPrelaunch.enabled", true);
// no second (QUIC) connection per host after the first TLS one
user_pref("network.http.http3.enable", false);
// load chrome/userContent.css (CSS animations finish at once)
user_pref("toolkit.legacyUserProfileCustomizations.stylesheets", true);
// the spare content process: Firefox skips it below 4 GB of RAM (this guest has 1.7 GB), and relaunches a
// replacement 1 s after one is used - right in the middle of the page load it was used for
user_pref("dom.ipc.processPrelaunch.lowmem_mb", 0);
user_pref("dom.ipc.processPrelaunch.delayMs", 60000);
// every new connection verified 2-3 extra signatures for Certificate Transparency and fetched/verified OCSP
// (security tradeoff: certificate revocation and CT logging are not checked; CRLite is already off)
user_pref("security.pki.certificate_transparency.mode", 0);
user_pref("security.OCSP.enabled", 0);
