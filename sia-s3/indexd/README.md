# indexd as run behind sia.enclave.host

Stock indexd v0.6.0 (SiaFoundation/indexd @ ddaf24b) plus `enclave-small.patch`, two constants sized for a
small indexer:

| constant | stock | ours | effect |
|---|---|---|---|
| `minContractGrowthRate` | 256 GiB | 8 GiB | each new contract is funded for this much upload+store+download growth |
| `serviceAccountFundTargetBytes` | 16 GiB | 1 GiB | per-host pre-funding of indexd's own service accounts |

Stock sizing locked ~1,277 SC into a single contract, about 500x our needs for ~5 GB across 30+ hosts.
Contracts simply get refreshed more often if we grow.

It also raises the application API's server timeouts (`cmd/indexd/run.go`) from 30 s read/write to 5 min
(30 s for headers, 5 min idle). With 30 s, a pin that ran long was cut mid-answer and sia-s3 failed the PUT with
`api error: http: i/o: early eof` (a 715 KB PUT failed 2 of 2 on 2026-10-16); with no IdleTimeout, Go also closed
keep-alive connections after 30 s idle, the length of one slab upload.

Build (same flags as the release):
```
git clone https://github.com/SiaFoundation/indexd && cd indexd && git checkout v0.6.0 && git apply enclave-small.patch
go generate ./... && CGO_ENABLED=0 go build -tags='netgo timetzdata' -trimpath -a -ldflags '-s -w' ./cmd/indexd
```
On nan: /opt/indexd/releases/v0.6.0-enclave2 (sha256 2d7a637e…e5dc, 2026-10-16); the contract-sizing-only build
v0.6.0-enclave1 (ade803e7…899eb) and the stock v0.6.0 are kept beside it.
Roll back: `ln -sfn /opt/indexd/releases/v0.6.0-enclave1 /opt/indexd/current && systemctl restart indexd`.
