# indexd as run behind sia.enclave.host

Stock indexd v0.6.0 (SiaFoundation/indexd @ ddaf24b) plus `enclave-small.patch`, two constants sized for a
small indexer:

| constant | stock | ours | effect |
|---|---|---|---|
| `minContractGrowthRate` | 256 GiB | 8 GiB | each new contract is funded for this much upload+store+download growth |
| `serviceAccountFundTargetBytes` | 16 GiB | 1 GiB | per-host pre-funding of indexd's own service accounts |

Stock sizing locked ~1,277 SC into a single contract, about 500x our needs for ~5 GB across 30+ hosts.
Contracts simply get refreshed more often if we grow.

Build (same flags as the release):
```
git clone https://github.com/SiaFoundation/indexd && cd indexd && git checkout v0.6.0 && git apply enclave-small.patch
go generate ./... && CGO_ENABLED=0 go build -tags='netgo timetzdata' -trimpath -a -ldflags '-s -w' ./cmd/indexd
```
On nan: /opt/indexd/releases/v0.6.0-enclave1 (sha256 ade803e7…899eb); stock binary kept at /opt/indexd/releases/v0.6.0.
Roll back: `ln -sfn /opt/indexd/releases/v0.6.0 /opt/indexd/current && systemctl restart indexd`.
