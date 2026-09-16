# Browser owner acceptance measurements, 2026-09-16

Acceptance remains open. The frozen candidate has a reproducible ServiceWorker
fallback panic, output retention grows with history, and local navigation is
slower than main. Passing unit tests alone did not expose these outcomes.

## Revisions and scope

| | Main | Candidate |
| --- | --- | --- |
| Source | `e3ae7c3bd131a6364d3dc11a6275a52ba8a3dcc3` | `dd6368eb87a85bf3f9110cff3475ed3d96ed3fcf` |
| Release SHA-256 | `b85d9c4b846b6e71c4a9bc864153a646f1c2190e766a090a73e974a2c48ffe3e` | `0e5b0db8c391783ecadfd4980647644dd536fdd1d16b043579c83c70e24bc3ee` |

Both used rustc 1.96.1, default release features, codegen-units=1 and the
repository's `-C link-arg=-Wl,--no-pie`, with independent Cargo targets. The V8
native archive SHA-256 was
`53677ea11387e3175b18c7c3338be3175abda5d8e9fb3afd3f84c80912e6b016`.
Main contains changes after the branches diverged; this compares revisions,
not an isolated treatment of the owner refactor.

Raw logs, wire traces, source patches, binary pins and all failed attempts are
under `target/smoke/split3-final-acceptance.pie713_t/`. They are local artifacts,
not included in this repository. The scripts below reproduce the local workload.

## Behavioral evidence

- Candidate: strict workspace Clippy, full nextest **18,498 passed / 13 configured
  skips**, 46 CDP groups / 487 scenarios, 165 WebDriver cases and three shared-page
  lifecycle probes passed. Main passed 165 WebDriver cases, but failed six CDP
  groups and the shared-page subscription probes. These differences remain in
  `public-baseline-comparison.json`.
- Full Lexbench: **1,928 tasks / 18 subsets**, one attempt, jobs=8. Main:
  **1,555 passed, 371 failed, 1 unsupported, 1 infra**. Candidate:
  **1,556 passed, 371 failed, 1 unsupported**. The two prior command-reply failures
  passed after the candidate fix. The only main-pass/candidate-fail task requires
  a background-color command added to main after the fork. The download task
  fails on both revisions. Both host reports recorded swap activity, so their
  approximately 655-second durations cannot certify a small performance change.
  The partial engine roster is not a formal multi-engine leaderboard.
- Full webfetch retained **all 259 addresses × 4 modes**, including failures
  excluded by the runner's default unreachable-site summary:

| Mode | Main passes / 259 | Candidate passes / 259 |
| --- | ---: | ---: |
| moli | 92 | 88 |
| moli-cdp | 88 | 86 |
| moli-full | 93 | 89 |
| moli-full-cdp | 89 | 83 |

All 20 status changes covered 11 addresses. A separate fixed three-attempt
diagnostic retained every result: 73/132 main successes and 72/132 candidate
successes. Public-network status variation prevents attributing every difference
to code. Two concrete candidate failures remain:

- Xiaohongshu triggered `ResourceTransfer::request` after terminal completion.
  The diagnostic captured the same panic in CLI and CDP, through
  `ServiceWorkerRuntimeService::dispatch_fetch_fallback`. Main had no panic.
- Slack full CDP disconnected **3/3**, while main passed **3/3**. An instrumented
  run reached the existing **1,536-message** transport budget at about 9 MiB.
  That run returned DCL before disconnecting; its benchmark pass does not erase
  the uninstrumented failures or establish that observation has no timing effect.

## Local timing and retained output

The loopback workload uses four pages, two warmup navigations per page, 25 measured
navigations per page, and four waves of 64 concurrent history reads. URLs and
console markers are unique. It requires the matching frame event before the
matching new-document console output.

Three balanced rounds without swap activity gave these navigation-command
medians: **main 1.66/1.55/1.67 ms; candidate 5.93/7.77/6.59 ms**. Frame-event
medians were **9.66/10.33/9.39 ms** and **13.62/14.68/13.96 ms** respectively.
These are wire measurements, not owner queue residence.

The 64 KiB buffered instrumented candidate measured command medians
**7.28/7.23/7.11 ms**. Its approximately 40,000 owner operations had median queue
wait **6.17–6.57 µs**, p95 **6.97–7.99 µs**, and observed waiting depth at most 1.
Local owner operations reached depth 3. Physical document-commit medians were
**4.95–7.13 µs**; the 108 exact sequence matches per round had first-projection
lag medians **1.25–1.66 ms**. There were no unmatched commits. These figures
include instrumentation overhead and initialization/warmup operations; observed
depth is not an unsampled high-water guarantee. Main has no equivalent owner
queue/fence, so those internal comparisons are unavailable.

A separate operation-count probe found **39,772 synchronous calls** before
retention work: 9,960 document lookups, 5,214 committed-navigation lookups,
4,684 context validations, 3,461 navigation snapshots and 3,300 context-level
document-commit snapshots. This identifies repeated reads for follow-up;
it does not establish which reads can safely be removed.

For retention, an unobserved SharedWorker emits 8 KiB log payloads in 24 batches
of 512, each followed by a diagnostics barrier. Candidate PSS at
0/4,096/8,192/12,288 records was **68.57/340.70/616.21/892.79 MiB**, despite its
Worker replay window staying near 10 MiB. Worker GC left **911.54 MiB**; closing
the creator page reduced it to **71.74 MiB**, and disposing the context to
**65.90 MiB**. Main reached **1,108.26 MiB** before GC and disconnected during
Runtime replay; later cleanup measurements were not reached.

Separate instrumented runs found small creator-page and Worker V8 heaps after
GC, while Rust allocation requests totaled **6.83 GB main / 7.08 GB candidate**
through 12,288 records. Allocation requests are not live allocated bytes and
exclude C++/V8. The source audit found duplicate Page report/console histories;
these have real diagnostics and CLI consumers, which must be preserved.

The distinct 512/1,536/2,048/4,096/4,096 burst workload disconnected on both
unmodified revisions. Candidate instrumentation recorded 67,081,892 queued bytes
plus a 33,914-byte record exceeding the existing 64 MiB transport budget.
Instrumentation sometimes changed overload outcomes. The original failures and
the first, overly expensive unbuffered trace measurements remain preserved.

## Reproduction

Run from the repository root with the benchmark environment installed. Use a
fresh output directory each time; `result.json` includes the binary hash, raw
samples and completion status, and `wire.json`/`server.log` retain failures.

```sh
PYTHONPATH="$PWD/moli-benchmark" moli-benchmark/.venv/bin/python -P \
  moli-benchmark/scripts/probe-browser-owner.py \
  --binary /absolute/path/to/pinned-moli --output /tmp/owner-probe-1 \
  --output-batches 512
```

Use `--output-batches` with 24 occurrences of `512` for the steady retention
workload, or `512 1536 2048 4096 4096` for the separate overload workload. The
probe checks the full live log sequence and contiguous replay tail. Do not turn
overload or replay failures into passing results by reducing a workload.

For internal measurements, create a detached worktree at the desired full SHA.
The instrumenter checks the revision, clean state and exact patch sites, and
refuses its own production checkout. It supports the two source layouts above;
changed layouts fail explicitly.

```sh
python3 -P moli-benchmark/scripts/instrument-browser-owner.py /tmp/owner-measured \
  --revision dd6368eb87a85bf3f9110cff3475ed3d96ed3fcf
git -C /tmp/owner-measured diff --binary > /tmp/owner-measurement.patch
CARGO_TARGET_DIR=/tmp/owner-measured-target RUSTFLAGS='-C link-arg=-Wl,--no-pie' \
  cargo build --release -p moli --manifest-path /tmp/owner-measured/Cargo.toml
```

Use `--baseline` for the main layout, or `--operation-counts` for a separate
candidate call-count investigation. Never share Rust build outputs between
revisions. Calibrate instrumented and unmodified pins in balanced rounds while
other builds/workloads are stopped. The allocator delegates to `System` and
counts successful Rust allocations/reallocations only. Trace output flushes at
diagnostic snapshots and admission rejection; shutdown after the last snapshot
may leave an unmeasured buffered tail.

Run the same probe against that binary, then summarize its raw trace:

```sh
python3 -P moli-benchmark/scripts/summarize-browser-owner-trace.py /tmp/owner-probe-1
```

The trace summary retains incomplete-probe status, matches commits by exact
BrowserSequence, reports missing matches, and separates queue wait, commit work,
projection lag, allocation requests and transport admission failures.
