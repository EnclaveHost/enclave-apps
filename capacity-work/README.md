# Capacity Work

A bounded CPU/RAM workload served through the ordinary WASI HTTP app interface.
Set a random `CAPACITY_WORK_TOKEN` (at least 32 characters) as a deployment secret.
Every request requires `Authorization: Bearer <token>`; missing configuration fails closed.
Deploy it through the normal catalog, isolation, shares and billing
path. POST a fresh random seed and work dimensions to `/v1/run`; independently
recompute with the native `reference` binary, and measure latency at the client.

This is explicitly identifiable verification work. Passing demonstrates the
work and allocation actually exercised, not exclusive hardware ownership or
all of a host's advertised RAM. RAM-residency claims require timing assumptions;
a sufficiently fast alternative implementation can trade compute for memory.
There is no GPU-capacity claim. No special runtime bypass, key, wallet or reward
logic is included in the component. Run several normal deployments concurrently
to exercise concurrent allocations. The scheduler must bound total purchased
shares, duration and spending and give customer work priority.

Build: `cargo build --release --target wasm32-wasip2 --lib`.
Reference: `cargo build --release --bin reference`.
Test: `cargo test --release`.

Example body: `{"seed":"<64 hex characters>","rounds":10000,"memory_mib":32,"passes":1}`.
Limits: 1–2,000,000 CPU rounds, 0–2,048 MiB memory, 1–8 memory passes, 4 KiB request.
Guest memory overhead is additional: configure the app's actual share accordingly.
Stage the secret before funding; share this job-specific token only with authorized
verifiers over authenticated channels. Never put it in on-chain app config. The response's counters alone
are never accepted as capacity evidence.
