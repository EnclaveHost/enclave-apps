# Alpine desktop image: Firefox start and page-load overlay

What the live RISC Box image (`alpine/rootfs-badwolf-20261002.ext2.gz`, "jit7", 2026-10-14) adds on top of the
desktop image with the Firefox ESR 140 JIT build installed. `apply-overlay.sh IN.ext2 OUT.ext2 libjemalloc.so.2`
writes `files/` into an image. Gzip the result with `gzip -6` (an image over ~1 GB gzipped does not fit the
app's memory), upload it to the `fs` key, then bake a new desktop snapshot. A snapshot is bound to the exact image.

| file | why |
|---|---|
| `usr/local/bin/risc-browser` | Starts Firefox light: tiny GTK theme, no accessibility bus, jemalloc preloaded, one shared D-Bus session. `--keep` runs a background Firefox with its window minimized, so a menu click only shows it. |
| `usr/local/bin/risc-open` | Hands a URL to the running Firefox over D-Bus (OpenURL). Without it, a whole `firefox-esr` remote client starts, costing ~20 s. |
| `usr/local/bin/risc-xwin` | Hides and shows the background Firefox's window (libX11 via ctypes). |
| `usr/bin/xdesktop.sh` | Desktop session loop; keeps the background Firefox running. |
| `root/.themes/RiscBox/gtk-3.0/gtk.css` | Adwaita's CSS parse goes through musl's soft-float long-double `strtod` and cost ~10 s at each start. |
| `usr/lib/firefox-esr/distribution/policies.json` | uBlock Origin: no auto-update. |
| profile `user.js` | Quiet startup (no telemetry, updates, Pocket, remote settings, Safe Browsing, crash reporting). Page load: AES-128-GCM suites off (ChaCha20 first: no AES hardware here), HTTP/3 off, process prelaunch on. Security tradeoffs: certificate transparency enforcement, OCSP and Safe Browsing are off. |
| profile `chrome/userContent.css` | Zero-duration CSS animations, and google.com's animated AI-Mode glow hidden (~30% CPU forever otherwise). |
| `libjemalloc.so.2` (Alpine `jemalloc` package, not in this tree) | ~12% fewer instructions to start than musl's allocator, and far fewer page faults. |

`apply-overlay.sh` also deletes the CRLite filters from the profile's `security_state/`. Firefox decoded the ~22 MB
of them at every start even with CRLite off.

Measured on warden-host (cold Firefox start, keeper stopped): ~133 s before these changes, ~79-88 s after.
Live on the NucBox, with the background Firefox kept by `risc-browser --keep`: the window shows in ~14 s, and
google.com loads ~75 s after Enter (61 s on the previous image, one sample each).
