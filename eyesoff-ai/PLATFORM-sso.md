# Sign in with Enclave: the platform half

The app half is DONE on this branch (0.51.0): `sso.rs` verifies tokens, the
playground runs the redirect flow, `/chat` and `/title` gate on a valid token
when the deployment says so, and `/v1/*` accepts one beside `api_key`. What
does not exist yet is the thing that MINTS tokens. This document is the
contract that side has to meet, written so it can be built in the platform
repo without reading the app.

## Why this shape

The platform's only identity is a wallet address; passkey and WalletConnect
are just two ways a session at enclave.host ends up able to speak for one.
Sharing the LOGIN therefore means sharing a claim about the address, not the
credential: a WalletConnect pairing is per dapp and a passkey is bound to its
RP, but a short-lived signed note saying "this address is signed in, for that
deployment, until then" can be carried by the browser to any app that knows
the platform's signing address.

The token is verified INSIDE the enclave, statelessly. No gateway injection
(the TLS path stays end to end), no introspection call (a deployment with no
egress can still gate), no session table (a wasip2 component keeps nothing
between requests). That puts three duties on the mint side: bind the
audience, keep the TTL short, and never let the signing key near anything
else, because expiry is the only revocation there is.

## Endpoint

```
GET https://enclave.host/sso/authorize
    ?aud=<0x + 64 hex, the deployment id>
    &redirect_uri=<absolute https URL>
    &state=<opaque, 1..256 chars>
    [&ttl=<seconds>]
```

Behavior:

1. **Session.** If the visitor has no live enclave.host session, run the
   normal login (passkey or WalletConnect) first, then continue. The flow is
   a plain navigation, so this is just the existing login page with a
   continuation.
2. **Validate `redirect_uri`.** Its origin must belong to the `aud`
   deployment: `https://<first-8-hex-of-aud>.app.enclave.host`, the
   deployment's registered custom domain(s), or the direct enclave origin
   serving `/x/<aud>`. Anything else is a hard error page, no redirect: a
   redirect to an unvalidated URI is a token exfiltration primitive. Compare
   origins exactly (scheme, host, port); the path may be anything, since the
   app returns to the page the visitor was on.
3. **Consent.** Show what is about to happen, first time per (account, aud)
   at minimum: the deployment id and origin being signed into, the address
   about to be named, an approve button. This is the phishing boundary: the
   page a visitor trusts with their passkey must say plainly who is asking.
4. **Mint** (format below) and 302 to:

```
<redirect_uri>#sso=<token>&state=<state, echoed verbatim>
```

The FRAGMENT, never the query: fragments do not reach servers, logs or
Referer headers. `state` is opaque to the platform; echo it byte for byte
(the app refuses a token whose state echo does not match the one it stored
when it started the flow, which is what stops a crafted link logging a
visitor into an attacker's account).

`ttl` clamps to `[300, 604800]`, default `86400` (24 h). The default is a
session length, not a security boundary; the security boundary is `aud`.

## Token format (EST1)

```
EST1.<base64url_nopad(claims JSON)>.<base64url_nopad(65-byte r||s||v signature)>
```

Claims, all required:

```json
{"v":1,"sub":"0x<40 hex, signed-in address, lowercase>",
 "aud":"0x<64 hex, deployment id>",
 "iat":<unix seconds>,"exp":<unix seconds>}
```

Signature: Ethereum **EIP-191 personal_sign** over the ASCII string
`EST1.<base64url(claims)>`, i.e.
`keccak256("\x19Ethereum Signed Message:\n" + len + "EST1.<b64>")` signed by
the platform SSO key, secp256k1 with recovery byte (`v` as 27/28 or 0/1, the
verifier takes both). The signed string is the token's own first two
segments, byte exact, so there is no canonical-JSON step to disagree about,
and any wallet tooling can mint a test token (`cast wallet sign` produces
exactly this signature).

Extra claims are ignored by the verifier, so `v:1` can grow fields without a
version bump; changing the MEANING of an existing field is what `v:2` is for.

## Test vector

Throwaway key `0x4242...42` (32 bytes of 0x42), whose address is
`0x17c5185167401ed00cf5f5b2fc97d9bbfdb7d025`. Claims:

```json
{"aud":"0xcc1f4f3f000000000000000000000000000000000000000000000000000000aa",
 "exp":1755086400,"iat":1755000000,
 "sub":"0x00a329c0648769a73afac7f9381e08fb43dbea72","v":1}
```

Token (one line):

```
EST1.eyJhdWQiOiIweGNjMWY0ZjNmMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwYWEiLCJleHAiOjE3NTUwODY0MDAsImlhdCI6MTc1NTAwMDAwMCwic3ViIjoiMHgwMGEzMjljMDY0ODc2OWE3M2FmYWM3ZjkzODFlMDhmYjQzZGJlYTcyIiwidiI6MX0.yk7Y_U0V-3ZyKhLJptbXZB3_Id-bEay1FtUTLFWjGgdyYQRL3xUJfKR5WTawlUSttKpUO0_H-x960Vn5-82NvRw
```

`sso.rs`'s `spec_vector` test mints and verifies exactly this (rerun with
`cargo test spec_vector -- --nocapture` to regenerate); a platform
implementation that produces this token for these claims and this key is
compatible.

## The signing key

- **Dedicated key.** Never the treasury or any key that signs transactions:
  this one signs whatever the authorize endpoint is asked to, on demand, and
  its blast radius must be "logins", not "funds".
- **Publish the address** at `https://enclave.host/.well-known/sso-signer.json`:

  ```json
  {"signer":"0x...","previous":["0x..."]}
  ```

  Operators pin it in deployment config (`sso.signer`); the app deliberately
  does NOT fetch it (a deployment with no egress must still verify), so
  rotation reaches deployments as a config update. `previous` exists so an
  operator who missed a rotation can see what changed.

## What the app expects (already built, for reference)

- Deployment config block, on-chain via configCid:

  ```json
  "sso": {"signer": "0x<platform address>",
          "audience": "0x<this deployment's id>",
          "required": true}
  ```

  plus optional `authorize_url` (defaults to the endpoint above) and
  `skew_secs` (default 300, applied to `iat` only; `exp` is strict).
- With `required` true: `POST /chat` and `POST /title` answer
  `401 {"error":{"message":...,"code":"sso_required"}}` without a valid
  token; `GET /`, `/models`, `/attestation` stay open (the page must be able
  to carry the sign-in button, learn where to send people, and be verified
  before being trusted with a login). `/v1/*` takes a valid token OR the
  deployment's `api_key`.
- The playground starts the flow with `aud`, `redirect_uri` (its own page)
  and a random `state` kept in sessionStorage, and stores the returned token
  in sessionStorage for the tab's lifetime.
- **Header transport caveat.** The fleet's inbound TLS proxy has been
  observed stripping `Authorization` (2026-08-17, deployment e64f7cba: a
  correct Bearer answered 401 while the same value as `X-Api-Key` answered
  200). The app therefore reads the credential from `Authorization: Bearer`
  OR `x-api-key`, and the playground sends both. Platform fix worth making
  regardless: the proxy should pass `Authorization` through untouched; until
  it does, `x-api-key` is the spelling that provably arrives.

## Non-goals, stated so they stay stated

- **Authorization.** The gate is "signed in with Enclave", not "allowed":
  any account satisfies it. An address allowlist beside `signer` is the
  obvious extension and belongs in the app when someone needs it.
- **Scopes / delegation.** The token means "may use this deployment", whole.
- **Server-side logout.** Dropping the token is logout; expiry is revocation.
- **The platform learning usage.** It learns that a login happened for a
  deployment (it served the login); conversations still never leave the
  enclave, and the app calls nothing out to verify.
