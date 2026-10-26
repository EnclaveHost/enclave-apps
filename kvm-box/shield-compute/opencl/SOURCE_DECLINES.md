# Optional source-rejection hints

This optimization is opt-in in the low-level frontend build. The 0.6.11 release
packager requires its bound build metadata. It is deployed and guest-validated
in the separate 0.6.11 development image. It reduces repeated compiler work for exact source/options requests
which the installed compiler declines. It never stores positive plans, data
permissions, layout approvals, or GPU results. An unknown request takes the
existing compiler path. A hit selects the existing trusted PoCL CPU fallback.

`build_source_declines.py` runs each supplied request twice using the installed
compiler. Six positive canaries cover integer GEMM and both Q8 references with
their supported flag profiles, before and after generation. Failed canaries,
unstable results and conflicting results for one fingerprint abort the build.
Canaries must return the expected named integer/Q8 plan schemas, arguments and
dispatchability, within the frontend output bound. Their exact result hashes
are recorded and must remain equal. The manifest also records the bytes
analyzed, compiler sources, Python/Clang and headers, inherited limits, and
relevant locale/temp-directory observations.

A canary is not proof that every negative result is semantic: per-source
resource failures can still look like declines. Hints must be regenerated when
compiler inputs or supported operations change. A false decline cannot grant
GPU work, but can lose acceleration and change approximate Q8 arithmetic to
native CPU arithmetic. Application output and offload-count checks remain
necessary. The explicit arithmetic contract and all runtime data, geometry,
dependency, failure and verification checks still apply to actual offloads.

The generated table uses SHA-256 over the versioned prefix
`shield-source-decline-v1` plus NUL, followed by each of source and options as
an unsigned 64-bit little-endian byte length and exact UTF-8 bytes. It performs
no whitespace, flag-order or source normalization. The C++ header checks sorted,
unique, lowercase 64-character fingerprints at compile time. The frontend
checks that the generated compiler path equals `SHIELD_COMPILER`. A hash or
allocation failure takes the original compiler path.

Build with `SHIELD_SOURCE_DECLINES` naming the generated `.inc` and link
`-lcrypto`. Use a fresh build directory when regenerating the table; the generic
Makefile does not discover an arbitrary macro-specified include dependency.
There is no runtime table path or environment override. The generated header
is build input with the same trust requirements as frontend source code.

The release workflow now enforces that binding. Generate a table with
`build_source_declines.py` inside the pinned target runtime image, with core
dumps disabled and the 8 MiB file bound. Then use
`native/build-declines-frontend.py --table TABLE --requests REQUESTS
--builder-image sha256:... --output BUILD`. This takes a source snapshot,
compiles the table-manifest digest into an ELF section, binds both frontend
and CPU loader, and records the build recipe, command, image and source hashes.
Use `--frontend-dir BUILD/runtime --source-declines-build BUILD` when staging
with `native/package-standard.py`. Bare frontends and pre-binding prototypes
are refused by this release packager. Historical 0.6.9/0.6.10 recipes predate
this required argument.

`native/source_declines_package.py` checks canonical table contents, corpus
membership, manifest/header/source/binary/command bindings and the embedded
ELF marker. During image finalization it checks installed compiler sources,
Python and Clang binaries/versions, and Clang resources. It then measures the
UID, no-new-privileges state, extra seccomp filter, VSOCK denial and file bound
of its sandbox child. Every captured request is reclassified twice, with
positive canaries before and after, at both the native-service 8 MiB file bound
and Clang's 32 MiB desktop ceiling. All results must match the bound manifest.

The gate uses an explicit environment; the runtime helper inherits its parent
application's sandbox and environment. Generation environment fields are
observations, not proof that every caller has identical settings. These checks
do not hash every transitive compiler dependency independently: the complete
immutable image remains the trust boundary, and actual classifications are
rechecked inside it. A malicious release builder is outside this guarantee.
Installed validation receipts omit timing fields; timings belong in external
build logs. Guest application and fault checks remain required after producing
the separately measured confidential-VM image.

Portable key tests are in `opencl/test_source_declines.py`. Real-artifact
mutation tests run with `native/test_source_declines_package.py --build BUILD
--baseline-frontend BASELINE_LIB`. Additional installed-image checks exercise
modified compiler/frontend/metadata and a passthrough sandbox replacement.
