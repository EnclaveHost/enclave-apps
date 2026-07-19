# enclave-apps

Example wasm apps intended to run on the [enclave.host](https://enclave.host)
platform. Each app is a self-contained Rust project compiled to a
`wasm32-wasip2` component and shaped to the platform's app contract — see each
app's README (and `wasm/apps/README.md` in the
[enclave](https://github.com/EnclaveHost/enclave) repo) for the details.

## Apps

| App | What it shows |
| --- | --- |
| [hello-world](hello-world/) | The minimal starting point: a `wasi:http` component that responds "Hello World!". |
| [network-test](network-test/) | "Who am I on the network?" — demonstrates dedicated-IP egress and declared ports. |
| [IRC](IRC/) | `nanircd`: a zero-dependency IRC server as a long-running TCP *service app*, reached through the WebSocket bridge. |
| [minecraft-server](minecraft-server/) | `nanmc`: a from-scratch Minecraft 1.8.9 (protocol 47) server — an ephemeral creative world, no JVM. |
| [nn-demo](nn-demo/) | Minimal end-to-end wasi-nn inference (bundled ONNX model). |
| [ggml-probe](ggml-probe/) | Smoke-tests the ggml (llama.cpp) wasi-nn backend end to end. |
| [llm-chat](llm-chat/) | OpenAI-compatible LLM chat service over wasi-nn; models come from attached read-only model volumes. |
| [image-generator](image-generator/) | Text-to-image on a GPU share via wasi-nn, serving host-preloaded models. |
| [encrypted-volumes](encrypted-volumes/) | User-held-key confidential storage: client-side rclone crypt over S3, unlocked in the enclave. |
| [vault](vault/) | The web UI for wallet-gated encrypted volumes (`wasi:http` component). |
| [dead-drop](dead-drop/) | Burn-after-reading secrets: browser-side AES-GCM, key in the URL fragment, ciphertext counted and erased in enclave RAM. |
| [hookbin](hookbin/) | Webhook/request inspector with a live SSE feed — captures exist in enclave RAM only. |
| [ballot](ballot/) | Anonymous polls sealed inside the enclave until close — the creator can't peek either. |
| [pixelboard](pixelboard/) | A shared 128×128 pixel canvas in enclave RAM: paint together while it's funded. |
| [handoff](handoff/) | Files through an attested enclave: chunked browser-side AES-GCM, erased on delivery. |
| [backchannel](backchannel/) | E2E-encrypted ephemeral chat rooms — the enclave relays ciphertext blind. |
| [warpad](warpad/) | E2E-encrypted shared scratchpad — every save replaces the only ciphertext; no history, anywhere. |
| [failsafe](failsafe/) | Time capsules and dead man's switches — ciphertext the enclave refuses to serve until the clock, or the silence, says so. |
| [pulse](pulse/) | Push-based uptime: cron jobs curl heartbeats into the enclave; status history nobody can edit. |
| [quorum](quorum/) | M-of-N secret release — break-glass escrow the enclave enforces; who approved stays private. |
| [fairdraw](fairdraw/) | Provably fair raffles: salt committed before entries, revealed at close, winners recomputable in your browser. |
| [tripwire](tripwire/) | Canary tokens with a live alarm board — the trip log is append-only in attested RAM, so an intruder can't erase the record of their own trip. |
| [tipline](tipline/) | Anonymous encrypted inbox — sources encrypt to your key in a page you can attest first. |

## Building

Every app builds the same way:

```bash
rustup target add wasm32-wasip2
cargo build --release --target wasm32-wasip2
```

Per-app READMEs cover local testing (what the platform actually runs, via
`wasmtime`) and how to publish/deploy on enclave.host.

Build artifacts (`target/`) and model weights (`model-volume/`) are not
committed; apps that need models document how to fetch them.

## License

[MIT](LICENSE)
