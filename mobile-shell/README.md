# mobile-shell

A verify-first mobile shell that wraps an Enclave app as an Android/iOS app -
the pilot wraps [eyesoff.ai](https://eyesoff.ai). Think Salesforce Mobile
Publisher, with one difference that is the whole point: **before the app
loads, the shell verifies on-device that the deployment behind it runs the
signed EnclaveHost/enclave release inside real enclave hardware** (quote
against the vendor root, Sigstore provenance for the pinned repo, measurement
match, TLS binding into the enclave origin - the same checks as
`enclave attest`).

## How it works

1. The webview opens a local, bundled splash (`www/index.html`). Nothing it
   executes comes from the network.
2. The splash fetches `<app>/attestation` to learn which enclave origin backs
   the app, then runs `@tinfoilsh/verifier` against that origin. The GitHub
   repo is **pinned at build time** and never read from the response.
3. On PASS the webview navigates to the app origin (`server.allowNavigation`
   keeps it, and only it, inside the webview). On FAIL the app does not load:
   the user sees the failed step and may retry, or explicitly continue
   unverified. A PASS is remembered for 24h so daily opens are instant.
4. Offline, the splash steps aside and the app's own service worker serves
   the cached shell.

Honest scope: a PASS proves the *deployment* is genuine. The app-domain TLS
leg still terminates at the platform relay until in-enclave app TLS ships;
after that, the shell is where certificate pinning against the attested key
belongs.

## White-label

Everything app-specific lives in `app.config.json` (name, appId, url, pinned
repo, colors). `npm run build` projects it into `capacitor.config.json` and
the splash config. A different customer app is a different `app.config.json`
plus icon art - the shell itself never changes. Store publishing follows the
Mobile Publisher model: each customer publishes under their **own** developer
accounts (Apple's template-app rule), with builds produced by CI.

## Camera

The wrapped app's composer offers "take a photo", which is an
`<input type="file" accept="image/*" capture="environment">`. The webview
hands that to the platform camera; no Capacitor camera plugin is involved and
none is needed.

Two platform details, both easy to get wrong in opposite directions:

- **iOS requires `NSCameraUsageDescription`** in `ios/App/App/Info.plist`.
  Without it, iOS does not degrade or prompt: it **terminates the app** the
  instant the camera is invoked. It is set.
- **Android must NOT declare `android.permission.CAMERA`.**
  `ACTION_IMAGE_CAPTURE` is serviced by the user's camera app and needs no
  permission from us, but Android has a trap: an app that *declares* the
  permission without holding it at runtime gets a `SecurityException` from
  that same intent. Declaring it "to be safe" is precisely what breaks it.
  The manifest carries only `INTERNET`, and the `FileProvider` +
  `res/xml/file_paths.xml` that Capacitor's file chooser needs to hand back
  the captured file. Leave all three alone.

## Building

```sh
npm ci
npm run build          # configure + bundle the splash
npm run test:live      # node, no CORS: full verify against the live app
npx cap sync           # copy web assets into android/ and ios/
```

- **Android**: CI builds a debug APK on every push (artifact on the workflow
  run), or open `android/` in Android Studio. Release builds need a signing
  key.
- **iOS**: needs a Mac or a macOS CI runner (`cd ios/App && pod install`,
  then Xcode). The project is generated and committed; icons are in place.

## Not yet built

- Passkey bridge (native WebAuthn shim) - needed for apps using platform
  session auth; eyesoff.ai has no login, so the pilot does not need it.
- Certificate pinning against the attested TLS key (blocked on in-enclave
  app TLS).
- Release signing + store publishing pipeline under customer accounts.
