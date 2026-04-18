# iperf3 Parity Roadmap

**Date:** 2026-04-18
**Status:** Design — approved through Section A; Sections B–H presented in auto mode.
**Scope:** Drive `rperf` from its current TCP-only MVP to full feature parity with real `iperf3`, preserving strict bidirectional wire-level interop.

## Goal

`rperf` today implements iperf3's TCP data path and control-channel state machine well enough for loopback self-tests and both-directions interop against real `iperf3`. This roadmap takes it to feature parity:

- Full UDP data path (jitter, loss, OOO).
- Reverse and bidirectional tests.
- Concurrent tests on a single server.
- Full iperf3 output parity (JSON, verbose, formatting).
- Full knob parity (`-w`, `-M`, `-C`, `-S`, `-Z`, `-n`, `-k`, `-T`, `-A`, TCP `-b`).
- Authentication (`--rsa-public-key` flow, iperf3-exact).

Runtime choice is deliberate: **stay on `std::thread` with blocking I/O.** No tokio. Network-benchmarking workloads are not thousand-connection workloads; iperf3 itself is synchronous. Keeping the runtime simple shortens the roadmap and reduces cross-sub-project dependencies.

Interop bar is deliberate: **strict wire compatibility with real iperf3 for every sub-project, in both directions.** The project's reason to exist per the README is being a drop-in replacement. Every sub-project's tests include cross-runs against the real `iperf3` binary.

## Ordering (six sub-projects)

| # | Name | Rationale for position |
|---|------|------------------------|
| 1 | UDP data path | Biggest new surface; least entanglement with existing TCP path; needed before reverse/bidir so the direction flag can be designed against both protocols. |
| 2 | Reverse + bidirectional | Adds a single direction flag to `ClientOptions` that both TCP and UDP data paths honor. Must follow #1 so flag is designed once. |
| 3 | Concurrent tests on server | Needs the session state machine stable across both protocols and both directions; those are what stabilize after #2. |
| 4 | JSON output + verbose + formatting | Output layer only, no protocol surface. Isolates cleanly and lands well once the data-path feature set is frozen. |
| 5 | Knob parity | Mostly socket-option + CLI plumbing; needs the output layer in place so new fields render consistently. |
| 6 | Authentication | Modifies the control-channel handshake — the riskiest wire change — so it lands last when the rest of the protocol is frozen. |

## Cross-cutting decisions (apply to all six)

- **Runtime:** `std::thread`, blocking I/O. No async.
- **Interop:** wire-format compatibility with real iperf3 in both directions is a hard requirement.
- **Module structure:** new protocol-specific code goes in new files under `src/common/` (`udp.rs`, `output/*.rs`, etc.). The existing `Socket` trait in `protocol.rs` extends to cover UDP rather than being duplicated.
- **Wire-compat test:** a golden-file test (`tests/wire_golden.rs`) captures the serialized bytes of every `Message` variant plus `ClientOptions` and `Results`, so any refactor that silently drifts from iperf3's format is caught.
- **Cross-interop tests:** each sub-project ships a `tests/cross_interop_<feature>.rs` that spawns the real `iperf3` binary, runs rperf-client↔iperf3-server and iperf3-client↔rperf-server, and skips gracefully with a `println!` when `iperf3` is not on `PATH`.
- **Error handling:** current `Box<dyn Error>` pattern continues; no migration to `anyhow`/`thiserror`.

## Out of scope for this roadmap

Named here explicitly rather than silently omitted. These may become future sub-projects:

- SCTP (`--sctp`)
- Multicast / flow label / `--extra-data`
- `-F <filename>` data source
- JSON streaming output (`--json-stream`)
- Daemon mode / pidfile
- Async runtime migration

---

## Sub-project 1 — UDP data path

**CLI additions:** `-u` (UDP mode), `-b <rate>` (bandwidth, default `1M` for UDP, `0` = unlimited).

**Wire format (iperf3-exact):**

Every datagram carries a 16-byte header (network byte order):

```
tv_sec:  u32
tv_usec: u32
seq:     i64
```

End-of-test sentinel: final packet from the sender has `seq < 0`.

**Receiver logic:**

- **Jitter** per RFC 1889: `J = J + (|D| - J) / 16` where `D = (R_j - R_i) - (S_j - S_i)` (R = receive time, S = sender timestamp from header). Reported in milliseconds.
- **Loss:** `expected = max_seq_seen + 1`; `lost = expected - received`.
- **Out of order:** any incoming seq strictly less than `max_seq_seen` increments OOO and is *not* counted as lost.
- **Packet count:** exposed as a `Results` field.

**Sender bandwidth pacing:**

Token bucket. At each send site: compute `expected_bytes_by_now = rate * elapsed`; if `bytes_sent > expected_bytes_by_now`, sleep until caught up. Coarse is fine — iperf3 itself does not promise packet-precise pacing.

**Components:**

- `src/common/udp.rs` (new) — `UdpStreamSocket` implementing `Socket`; 16-byte header codec.
- `src/common/stream.rs` — stream worker branches on `protocol: Tcp | Udp`.
- `src/common/test.rs` — `StreamReceipt` adds `jitter_ms: f64`, `lost: u64`, `ooo: u64`, `packets: u64`.
- `src/common/wire.rs` — `ClientOptions` adds `udp: bool`, `bandwidth: u64`. `Results` adds the four UDP receipt fields.
- `src/cli.rs` — `-u`, `-b`.

**Tests:**

- `tests/self_test.rs` grows a UDP loopback case (asserts jitter ≥ 0, OOO = 0 on loopback, lost = 0 on loopback).
- `tests/cross_interop_udp.rs` — iperf3-client↔rperf-server and rperf-client↔iperf3-server UDP, both at `-b 100M`, assert exit 0 and that both sides report nonzero throughput, ≤ expected loss.

---

## Sub-project 2 — Reverse + bidirectional

**CLI additions:** `-R`/`--reverse`, `--bidir` (mutually exclusive with `-R`).

**Wire:** `ClientOptions` adds `reverse: bool`, `bidirectional: bool`. Control-channel direction unchanged (client still initiates).

**Role model:** introduce `Direction = Forward | Reverse | Bidirectional` in `Config`. The stream worker becomes symmetric with a `send_loop` and `recv_loop`; `Forward` runs client-side send + server-side recv, `Reverse` swaps, `Bidirectional` runs both on the same socket.

**Components:**

- `src/common/test.rs` — `Config.direction`.
- `src/common/stream.rs` — worker dispatches to send/recv/both based on direction; works for both TCP and UDP (this sub-project lands after UDP so both paths get direction support at once).
- `src/cli.rs` — `-R`, `--bidir`.
- `src/client.rs`, `src/server.rs` — role assignment at test start.

**Tests:**

- Self-test cases for reverse TCP, reverse UDP, bidir TCP, bidir UDP.
- `tests/cross_interop_reverse.rs` — cross-run `-R` and `--bidir` against real iperf3 in both directions.

---

## Sub-project 3 — Concurrent tests on server

**CLI additions:** `-1` (iperf3-compat: server exits after one test); `--max-concurrent N` (rperf extension; default unlimited).

**Approach:** sessions share the single listen port. Data streams demultiplex by cookie (already supported by `cookie.rs`). Remove the "one test at a time" guard in `server.rs`.

**Session manager:** `Arc<Mutex<HashMap<Cookie, SessionHandle>>>`. The accept loop spawns a new `Session` thread per control connection; the session registers its cookie on ParamExchange, owns its own stream-worker threads, and deregisters on IperfDone. Data-stream acceptor threads read the cookie from each incoming stream and hand the stream to the matching session via its `SessionHandle`.

**Back-pressure:** when `--max-concurrent` is hit, the server responds on the control channel with iperf3's exact "the server is busy" error format so real iperf3 clients surface the right message.

**Components:**

- `src/server.rs` — refactored from "single test" to "accept loop + session map".
- `src/common/session.rs` (new) — `Session` and `SessionHandle`.
- `src/cli.rs` — `-1`, `--max-concurrent`.

**Tests:**

- Self-test: spin up server, fire five concurrent clients, assert all five complete with distinct receipts.
- Cross-interop: run rperf-server with three concurrent iperf3-clients; assert all three succeed.
- Back-pressure test: `--max-concurrent 1`, second client gets "the server is busy".

---

## Sub-project 4 — JSON output + verbose + formatting

**CLI additions:** `-J` (JSON), `-V` (verbose), `-f <unit>` (format), `--logfile <file>`, `--timestamps[=FMT]`.

**Structure:** introduce a `Reporter` trait in `src/output/mod.rs` with `start()`, `interval(row)`, `stream_summary(stream)`, `end(summary)`. Two implementations: `src/output/text.rs` (current behavior) and `src/output/json.rs` (iperf3-shaped JSON: `start {}`, `intervals: [{streams: [...], sum: {...}}]`, `end {}`).

Every `println!` in `client.rs` and `server.rs` migrates to a reporter call. The reporter owns `--logfile` redirection and `--timestamps` prefixing.

**Parity testing:** capture real iperf3's JSON output for a known-good run; golden-file diff rperf's JSON structurally (field presence and types, not exact values — throughput varies run to run).

**Components:**

- `src/output/mod.rs`, `src/output/text.rs`, `src/output/json.rs` (new).
- `src/client.rs`, `src/server.rs` — route all printing through the reporter.
- `src/cli.rs` — `-J`, `-V`, `-f`, `--logfile`, `--timestamps`.

**Tests:**

- JSON structural golden test against a captured iperf3 JSON fixture.
- Verbose text-output snapshot test.

---

## Sub-project 5 — Knob parity

Each knob is a small independent slice: CLI flag → socket-option call (and/or `ClientOptions` field for server-side effect) → test.

| Flag | Effect | Platform |
|------|--------|----------|
| `-w <size>` | `SO_SNDBUF`/`SO_RCVBUF`; print negotiated size after `getsockopt` | all |
| `-M <mss>` | `TCP_MAXSEG` | Linux |
| `-C <algo>` | `TCP_CONGESTION` (string setsockopt) | Linux |
| `-S <tos>` | `IP_TOS` | all |
| `-Z` | `sendfile` zero-copy on the send path | Linux |
| `-n <bytes>` | byte-count test termination (mutually exclusive with `-t`) | all |
| `-k <blocks>` | block-count termination | all |
| `-T <title>` | prefix on output lines | all |
| `-A <list>` | `sched_setaffinity` per stream worker | Linux |
| `-b <rate>` (TCP) | TCP-side bandwidth cap via same token bucket as UDP | all |

Each knob that has a server-side visible effect adds a matching `ClientOptions` field so the server applies it when set by the client.

Non-Linux: platform-gated behavior. Unsupported flags either refuse with a clear error (`-C`, `-M`, `-A`) or log "ignored on this platform" (`-Z`). Choice per flag documented in the implementation plan.

**Tests:**

- Each flag: a small targeted test that sets it and asserts the underlying socket option or behavior change.
- Cross-interop: pick a representative non-trivial combo (`-w 256K -S 0x10`) and cross-run against iperf3.

---

## Sub-project 6 — Authentication

**iperf3-exact scheme:**

- Client: `--rsa-public-key <pem>` loads the server's public key. `--username` + `--password` (prompted via `rpassword` if unset).
- Client builds `"{timestamp}\n{username}\n{sha256-hex(password)}"`, encrypts with RSA-OAEP (SHA-256), base64-encodes, and places in `ClientOptions.authtoken`.
- Server: `--rsa-private-key <pem>` + `--authorized-users <csv>` (each row `username,sha256hex(password)`). Decrypts `authtoken`, validates timestamp skew ≤ 10s, looks up `(username, hash)` in the file.

**New dependencies:** `rsa`, `sha2`, `base64`, `rpassword`.

**Error handling:** auth failure returns iperf3's exact control-channel error string so a real iperf3 client surfaces the standard message.

**Components:**

- `src/common/auth.rs` (new) — encrypt side (client) and decrypt+validate side (server).
- `src/common/wire.rs` — `ClientOptions.authtoken: Option<String>`.
- `src/cli.rs` — `--rsa-public-key`, `--rsa-private-key`, `--authorized-users`, `--username`, `--password`.

**Tests:**

- Round-trip unit test with a fixture keypair: encrypt → decrypt → validate.
- Cross-interop: generate a test keypair, authenticate rperf-client to real iperf3-server and iperf3-client to rperf-server.
- Negative tests: bad password, stale timestamp, unknown user.

---

## Testing strategy (cross-cutting)

Per sub-project, three layers:

1. **Unit tests** for new pure logic: UDP header codec, jitter EWMA, JSON formatter, RSA round-trip.
2. **Self-tests** in `tests/self_test.rs` (in-process client↔server loopback), extended with new cases per sub-project.
3. **Cross-interop tests** in a new `tests/cross_interop_<feature>.rs` per sub-project. Each spawns real `iperf3` via `std::process::Command`, runs both directions, asserts exit code and output-parse. Skips with `println!("skipping cross-interop: iperf3 not on PATH")` when the binary is absent, so contributors without iperf3 are not blocked.

One more cross-cutting test:

- `tests/wire_golden.rs` — serialize every `Message` variant, plus `ClientOptions` and `Results`, and byte-diff against a committed fixture. A sub-project cannot silently alter the wire format without updating the fixture deliberately, which a reviewer will notice.

**CI cadence:** `cargo test` runs all three layers; cross-interop tests auto-skip in environments without iperf3.

---

## Deliverables per sub-project

Each sub-project, when picked up, produces:

1. An implementation plan at `docs/superpowers/plans/YYYY-MM-DD-<sub-project>-plan.md` via the `writing-plans` skill, based on the corresponding section in this roadmap.
2. A fresh spec file at `docs/superpowers/specs/YYYY-MM-DD-<sub-project>-design.md` **only if** context has drifted from the roadmap section (new requirements, changed dependencies, protocol revisions). Otherwise the roadmap section is the spec.
3. Implementation, landing on `master` (or feature branches per user preference at that time).
4. README "What works today" / "Out of scope" updates to reflect the new surface.

The roadmap itself (this document) is the stable reference that survives between sub-projects. The first sub-project to be planned is UDP data path (sub-project 1), from Section "Sub-project 1 — UDP data path" above.
