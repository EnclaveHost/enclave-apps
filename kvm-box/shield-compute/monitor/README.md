# Shield GPU monitoring in nvtop

This is a pinned nvtop source build with an Enclave Shield vendor backend. It
enumerates the logical device used by the guest's compute frontends. It does
not emulate an NVIDIA device, implement NVML, or prove general CUDA compatibility.
Stock nvtop does not recognize this service; the VM needs this packaged build.

Run `nvtop` for the terminal interface or `nvtop --snapshot` for JSON. The extra
information row is enabled by default in this build:

- `State`: ready, busy, retired, or offline (telemetry unavailable).
- `Jobs`: successfully completed service jobs, including CPU-only work.
- `Exch`: verified remote exchanges belonging to successful jobs.
- `MACs`: logical field multiply-accumulates in those verified exchanges.

The terminal abbreviates large counters with decimal k/M/G/T/P/E suffixes;
snapshot JSON preserves exact integer counters under `shield`. Read counters as
integers, not JavaScript floating-point numbers if they exceed 2^53. Counts
reset when the service restarts. In-flight jobs and verified chunks of failed
jobs are excluded from these successful-job totals. Busy describes the whole
service job, including CPU preparation and fallback; it is not GPU utilization.

Physical utilization, VRAM residency, clocks, power, temperature and per-process
GPU attribution are unavailable and remain N/A/null. No percentage, physical
memory capacity, or process activity is invented from service counters. Graphs
for unavailable measurements convey no utilization evidence.

The backend reads only the fixed guest Unix socket, submits only `status`, and
checks Linux SO_PEERCRED for service UID 1001. It has no worker address override,
no raw GPU handle, no external helper, and no compute operation. Replies are
bounded to 4 KiB with a 750 ms total I/O deadline and depth-limited strict JSON.
Malformed or unavailable telemetry clears old values. Initialization without
valid telemetry reports no device; an existing monitor shows offline after a
later failure. Restart nvtop after restoring an initially unavailable service.
This is read-only telemetry within the already trusted guest; it is not a new
remote attestation protocol or a guarantee against root inside the VM.

## Build and test

Build the supplied Dockerfile as `enclave-kvm-shield-monitor-builder:1`, obtain
the official nvtop checkout, then run:

```sh
python3 build.py --source /path/to/nvtop --output /path/to/new-build
```

The script archives commit `71094a9f7dfbfe7cbe9faebfdf055e580a034283`, applies
`nvtop.patch`, adds the Shield backend, and builds with all physical GPU vendors
disabled. Compilation runs with 1 CPU/1 GiB and no network. JSON-C is linked
statically; the Debian guest supplies ncurses/tinfo and libc. The output includes
the full corresponding source, license, build recipe, dependency versions and
artifact hashes. Copy `package/` into a new separately measured desktop image.

`test_monitor.py` runs only inside a disposable root Docker container with no
network or GPU, using a synthetic UID-1001 Unix server. Tests exercise the actual
nvtop snapshot executable, not a substitute parser. They cover ready/busy/retired
states, exact large integers, malformed and oversized replies, wrong peer UID,
bounded stalls, stale-value clearing, and absence of fabricated physical metrics.
These fixture tests are separate from validation in the attested desktop.
