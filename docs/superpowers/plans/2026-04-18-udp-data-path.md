# UDP Data Path Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add full iperf3-compatible UDP data-path support to rPerf3 (client and server), including jitter/loss/out-of-order accounting, the `-u` and `-b` CLI flags, bandwidth pacing, and cross-iperf3 interop.

**Architecture:** UDP uses the existing TCP control channel unchanged. `ClientOptions` grows `udp: bool` and `bandwidth: u64` so the server branches on protocol at CreateStreams time. Client-side, each parallel stream opens a connected UDP socket, sends the 37-byte cookie as the first datagram, then sends 16-byte-headered datagrams paced by a token bucket. Server-side, one UDP socket is bound to the test port; a demux loop keys per-stream state by source `SocketAddr`. Test termination uses a negative-seq sentinel datagram per stream plus the existing `TestEnd` control byte. No async — everything runs on `std::thread` to match the established pattern.

**Tech Stack:** Rust 2021, `std::net::UdpSocket`, `std::thread`, `clap` (existing), `serde`/`serde_json` (existing). No new crates.

---

## File Structure

**Files created:**
- `src/common/udp_header.rs` — 16-byte iperf3 UDP datagram header codec.
- `src/common/pacing.rs` — `TokenBucket` struct for sender-side bandwidth pacing.
- `src/common/jitter.rs` — RFC 1889 jitter EWMA helper.
- `src/common/udp_session.rs` — server-side UDP bind, cookie-demux, per-stream receiver loop.
- `src/common/bandwidth.rs` — `-b` rate-string parser (`"1M"`, `"200K"`, `"1G"`, `"0"` for unlimited).
- `tests/cross_interop_udp.rs` — integration test against real `iperf3` binary (both directions).

**Files modified:**
- `src/common/mod.rs` — expose new submodules; add `TransportKind` enum.
- `src/common/wire.rs` — `ClientOptions` gains `udp`, `bandwidth`; both with `#[serde(default)]` for backward-compat.
- `src/common/test.rs` — `Config` gains `transport: TransportKind`, `bandwidth: u64`; `ClientStreamReceipt` gains `jitter_ms`, `lost`, `ooo`, `packets`.
- `src/common/stream.rs` — new `Stream::start_udp` alongside existing TCP `start`.
- `src/server.rs` — `StreamReceipt` gains UDP fields; `serve_one` branches on `opts.udp` to call the UDP path; `build_server_results` populates jitter/lost/packets.
- `src/client.rs` — `create_streams` branches on `test.config.transport`; `build_client_results` populates UDP fields.
- `src/cli.rs` — `-u/--udp` and `-b/--bandwidth` flags; parse into `TransportKind` + `bandwidth`.
- `src/lib.rs` — no change (public API stays stable).
- `tests/self_test.rs` — add UDP loopback case.
- `README.md` — move UDP from "Out of scope" to "What works today"; document `-u` and `-b`.

Each file has a single responsibility. No module is expected to grow above ~400 lines; where `server.rs` risks bloat (it's already ~960 lines including tests), the UDP receiver logic lives in `src/common/udp_session.rs` and is called from a small branch in `serve_one`.

---

## Task 1: Add `udp` and `bandwidth` fields to ClientOptions

**Files:**
- Modify: `src/common/wire.rs`
- Test: `src/common/wire.rs` (existing `#[cfg(test)] mod tests`)

**Context:** `ClientOptions` is serialized as JSON over the control channel during ParamExchange. iperf3 clients/servers that don't know about our new fields must still parse the payload (Serde ignores unknown fields by default on deserialize, but we must make *our* deserializer accept payloads *without* the new fields — hence `#[serde(default)]`).

- [ ] **Step 1: Write the failing test**

Append to `src/common/wire.rs` tests module:

```rust
#[test]
fn options_defaults_udp_false_and_bandwidth_zero() {
    let opts = ClientOptions::tcp_defaults(1, 1, DEFAULT_TCP_LEN as u32);
    assert_eq!(opts.udp, false);
    assert_eq!(opts.bandwidth, 0);
}

#[test]
fn options_udp_roundtrip() {
    let mut opts = ClientOptions::tcp_defaults(1, 1, DEFAULT_TCP_LEN as u32);
    opts.udp = true;
    opts.bandwidth = 1_000_000;

    let json = serde_json::to_string(&opts).unwrap();
    assert!(json.contains("\"udp\":true"));
    assert!(json.contains("\"bandwidth\":1000000"));

    let back: ClientOptions = serde_json::from_str(&json).unwrap();
    assert_eq!(back, opts);
}

#[test]
fn options_parses_legacy_payload_without_udp_or_bandwidth() {
    // Shape of a ClientOptions from a rPerf3 version that predates UDP.
    let legacy = r#"{"tcp":true,"omit":0,"time":5,"parallel":1,"len":131072,"client_version":"3.1.3"}"#;
    let opts: ClientOptions = serde_json::from_str(legacy).expect("parse legacy");
    assert_eq!(opts.udp, false);
    assert_eq!(opts.bandwidth, 0);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib wire::tests::options_udp_roundtrip wire::tests::options_defaults_udp_false_and_bandwidth_zero wire::tests::options_parses_legacy_payload_without_udp_or_bandwidth`
Expected: compilation failure — `no field udp on type ClientOptions`.

- [ ] **Step 3: Implement — extend `ClientOptions`**

In `src/common/wire.rs`, change `ClientOptions`:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientOptions {
    pub tcp: bool,
    pub omit: u32,
    pub time: u32,
    pub parallel: u32,
    pub len: u32,
    pub client_version: String,
    #[serde(default)]
    pub udp: bool,
    #[serde(default)]
    pub bandwidth: u64,
}
```

And update `tcp_defaults` to initialize the new fields:

```rust
impl ClientOptions {
    pub fn tcp_defaults(time_secs: u32, parallel: u32, len: u32) -> Self {
        Self {
            tcp: true,
            omit: 0,
            time: time_secs,
            parallel,
            len,
            client_version: CLIENT_VERSION.to_string(),
            udp: false,
            bandwidth: 0,
        }
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib wire::tests`
Expected: all wire tests pass.

- [ ] **Step 5: Commit**

```bash
git add src/common/wire.rs
git commit -m "wire: extend ClientOptions with udp and bandwidth fields"
```

---

## Task 2: UDP datagram header codec

**Files:**
- Create: `src/common/udp_header.rs`
- Modify: `src/common/mod.rs` (declare new submodule)
- Test: `src/common/udp_header.rs`

**Context:** iperf3 UDP datagrams start with a 16-byte header in network byte order: `tv_sec: u32`, `tv_usec: u32`, `seq: i64`. A final datagram with `seq < 0` signals end-of-test. Sender stamps each packet with monotonic seq and wall-clock send time; receiver uses both (send timestamp for jitter; seq for loss/OOO).

- [ ] **Step 1: Create file with failing tests**

Create `src/common/udp_header.rs`:

```rust
//! iperf3-compatible UDP datagram header: 16 bytes, network byte order.
//!
//! Layout:
//! - bytes 0..4  : tv_sec  (u32, BE) — sender wall-clock seconds
//! - bytes 4..8  : tv_usec (u32, BE) — sender wall-clock microseconds
//! - bytes 8..16 : seq     (i64, BE) — monotonic sequence number; negative = end-of-test

use std::io;

pub const UDP_HEADER_LEN: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UdpHeader {
    pub tv_sec: u32,
    pub tv_usec: u32,
    pub seq: i64,
}

impl UdpHeader {
    pub fn encode(self, out: &mut [u8]) -> io::Result<()> {
        if out.len() < UDP_HEADER_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "buffer too small for UDP header",
            ));
        }
        out[0..4].copy_from_slice(&self.tv_sec.to_be_bytes());
        out[4..8].copy_from_slice(&self.tv_usec.to_be_bytes());
        out[8..16].copy_from_slice(&self.seq.to_be_bytes());
        Ok(())
    }

    pub fn decode(buf: &[u8]) -> io::Result<Self> {
        if buf.len() < UDP_HEADER_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "datagram shorter than 16-byte UDP header",
            ));
        }
        let tv_sec = u32::from_be_bytes(buf[0..4].try_into().unwrap());
        let tv_usec = u32::from_be_bytes(buf[4..8].try_into().unwrap());
        let seq = i64::from_be_bytes(buf[8..16].try_into().unwrap());
        Ok(Self { tv_sec, tv_usec, seq })
    }

    pub fn is_sentinel(&self) -> bool {
        self.seq < 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_roundtrip() {
        let h = UdpHeader { tv_sec: 0x11223344, tv_usec: 0x55667788, seq: 0x0A0B_0C0D_1234_5678 };
        let mut buf = [0u8; 16];
        h.encode(&mut buf).unwrap();
        let back = UdpHeader::decode(&buf).unwrap();
        assert_eq!(h, back);
    }

    #[test]
    fn encode_layout_is_big_endian() {
        let h = UdpHeader { tv_sec: 1, tv_usec: 2, seq: 3 };
        let mut buf = [0u8; 16];
        h.encode(&mut buf).unwrap();
        assert_eq!(&buf[0..4], &[0, 0, 0, 1]);
        assert_eq!(&buf[4..8], &[0, 0, 0, 2]);
        assert_eq!(&buf[8..16], &[0, 0, 0, 0, 0, 0, 0, 3]);
    }

    #[test]
    fn decode_rejects_short_buffer() {
        let short = [0u8; 8];
        let err = UdpHeader::decode(&short).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn encode_rejects_short_buffer() {
        let h = UdpHeader { tv_sec: 0, tv_usec: 0, seq: 0 };
        let mut tiny = [0u8; 4];
        let err = h.encode(&mut tiny).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn sentinel_is_negative_seq() {
        let h = UdpHeader { tv_sec: 0, tv_usec: 0, seq: -1 };
        assert!(h.is_sentinel());
        let not = UdpHeader { tv_sec: 0, tv_usec: 0, seq: 0 };
        assert!(!not.is_sentinel());
    }

    #[test]
    fn decode_negative_seq_roundtrips() {
        let h = UdpHeader { tv_sec: 10, tv_usec: 20, seq: -42 };
        let mut buf = [0u8; 16];
        h.encode(&mut buf).unwrap();
        let back = UdpHeader::decode(&buf).unwrap();
        assert_eq!(back.seq, -42);
        assert!(back.is_sentinel());
    }
}
```

- [ ] **Step 2: Declare the submodule**

Edit `src/common/mod.rs` — add `pub mod udp_header;` alongside the other `pub mod` lines.

- [ ] **Step 3: Run tests**

Run: `cargo test --lib udp_header`
Expected: 6 tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/common/udp_header.rs src/common/mod.rs
git commit -m "udp: 16-byte iperf3-compatible datagram header codec"
```

---

## Task 3: Token-bucket bandwidth pacing

**Files:**
- Create: `src/common/pacing.rs`
- Modify: `src/common/mod.rs` (declare submodule)
- Test: `src/common/pacing.rs`

**Context:** iperf3 in UDP mode defaults to 1 Mbps and throttles sender via a token bucket. We'll match that behavior. `bandwidth == 0` means unlimited. The pacer is pure: `wait_if_needed` returns a `Duration` to sleep (caller decides whether to actually sleep). Unit: `rate_bps` is *bits* per second (matches iperf3's `-b` flag).

- [ ] **Step 1: Create file with failing tests**

Create `src/common/pacing.rs`:

```rust
//! Token-bucket sender pacing for UDP/TCP bandwidth limits.
//!
//! Tracks bytes-sent vs. expected-bytes-at-this-time and returns a
//! `Duration` to sleep when the sender is ahead of schedule. `rate_bps`
//! is bits per second, matching iperf3's `-b` flag. A rate of 0 disables
//! pacing (unlimited).

use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
pub struct TokenBucket {
    pub rate_bps: u64,
    pub bytes_sent: u64,
    pub start: Instant,
}

impl TokenBucket {
    pub fn new(rate_bps: u64, start: Instant) -> Self {
        Self { rate_bps, bytes_sent: 0, start }
    }

    /// Record `n` bytes as sent. No-op when unlimited.
    pub fn record(&mut self, n: u64) {
        self.bytes_sent = self.bytes_sent.saturating_add(n);
    }

    /// How long to sleep (if at all) given the current wall-clock `now`.
    /// Zero or unlimited rate returns `None`. Otherwise returns `Some(d)`
    /// where `d > 0` means "sleep d before the next send."
    pub fn wait(&self, now: Instant) -> Option<Duration> {
        if self.rate_bps == 0 {
            return None;
        }
        let elapsed = now.duration_since(self.start).as_secs_f64();
        let expected_bytes = (self.rate_bps as f64 / 8.0) * elapsed;
        let ahead = self.bytes_sent as f64 - expected_bytes;
        if ahead <= 0.0 {
            return None;
        }
        let seconds_to_wait = ahead / (self.rate_bps as f64 / 8.0);
        Some(Duration::from_secs_f64(seconds_to_wait))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlimited_rate_never_waits() {
        let start = Instant::now();
        let mut bucket = TokenBucket::new(0, start);
        bucket.record(10_000_000);
        assert!(bucket.wait(start).is_none());
        assert!(bucket.wait(start + Duration::from_secs(60)).is_none());
    }

    #[test]
    fn no_wait_when_exactly_on_schedule() {
        let start = Instant::now();
        // 1 Mbps = 125_000 bytes/sec. After 1s, expected = 125_000.
        let mut bucket = TokenBucket::new(1_000_000, start);
        bucket.record(125_000);
        assert!(bucket.wait(start + Duration::from_secs(1)).is_none());
    }

    #[test]
    fn wait_proportional_when_ahead() {
        let start = Instant::now();
        // 1 Mbps: rate_bytes = 125_000. Sent 250_000 at t=1s => 1s ahead.
        let mut bucket = TokenBucket::new(1_000_000, start);
        bucket.record(250_000);
        let d = bucket.wait(start + Duration::from_secs(1)).expect("should wait");
        // Expect ~1 second of sleep (allow ±50ms tolerance).
        let ms = d.as_millis() as i64;
        assert!((950..=1_050).contains(&ms), "unexpected wait: {} ms", ms);
    }

    #[test]
    fn no_wait_when_behind_schedule() {
        let start = Instant::now();
        let mut bucket = TokenBucket::new(1_000_000, start);
        bucket.record(1_000); // miles behind
        assert!(bucket.wait(start + Duration::from_secs(1)).is_none());
    }

    #[test]
    fn record_is_saturating() {
        let start = Instant::now();
        let mut bucket = TokenBucket::new(1_000_000, start);
        bucket.bytes_sent = u64::MAX - 10;
        bucket.record(1_000_000);
        assert_eq!(bucket.bytes_sent, u64::MAX);
    }
}
```

- [ ] **Step 2: Declare submodule**

Edit `src/common/mod.rs` — add `pub mod pacing;`.

- [ ] **Step 3: Run tests**

Run: `cargo test --lib pacing`
Expected: 5 tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/common/pacing.rs src/common/mod.rs
git commit -m "pacing: token-bucket bandwidth limiter for UDP senders"
```

---

## Task 4: RFC 1889 jitter EWMA helper

**Files:**
- Create: `src/common/jitter.rs`
- Modify: `src/common/mod.rs`
- Test: `src/common/jitter.rs`

**Context:** Jitter per RFC 1889: `J = J + (|D| - J) / 16` where `D = (R_j - R_i) - (S_j - S_i)`. R = receive time, S = sender timestamp. We keep it as a pure function so the caller owns the previous `J` and both timestamps.

- [ ] **Step 1: Create file with failing tests**

Create `src/common/jitter.rs`:

```rust
//! RFC 1889 jitter EWMA. Pure function so callers own all state.
//!
//! For each packet after the first, compute
//!     D = (R_j - R_i) - (S_j - S_i)
//! where R is local receive time and S is sender-stamped send time (both
//! in the same unit, e.g. milliseconds). Then update:
//!     J = J + (|D| - J) / 16

/// Update the jitter estimate given the previous jitter value and the
/// transit-time difference `d` between two consecutive packets (same
/// units; must be consistent). Returns the new jitter in the same unit.
pub fn update(prev_jitter: f64, d: f64) -> f64 {
    prev_jitter + (d.abs() - prev_jitter) / 16.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_d_leaves_jitter_unchanged() {
        assert_eq!(update(5.0, 0.0), 5.0 - 5.0 / 16.0);
    }

    #[test]
    fn converges_to_d_under_constant_input() {
        // With |D| = 10 forever, J asymptotes to 10.
        let mut j = 0.0;
        for _ in 0..200 {
            j = update(j, 10.0);
        }
        assert!((j - 10.0).abs() < 0.01, "did not converge: {}", j);
    }

    #[test]
    fn absolute_value_is_used() {
        let pos = update(1.0, 5.0);
        let neg = update(1.0, -5.0);
        assert_eq!(pos, neg);
    }

    #[test]
    fn starting_from_zero_moves_toward_d() {
        let j = update(0.0, 16.0);
        assert_eq!(j, 1.0);
    }
}
```

- [ ] **Step 2: Declare submodule**

Edit `src/common/mod.rs` — add `pub mod jitter;`.

- [ ] **Step 3: Run tests**

Run: `cargo test --lib jitter`
Expected: 4 tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/common/jitter.rs src/common/mod.rs
git commit -m "jitter: RFC 1889 EWMA helper"
```

---

## Task 5: Bandwidth rate-string parser

**Files:**
- Create: `src/common/bandwidth.rs`
- Modify: `src/common/mod.rs`
- Test: `src/common/bandwidth.rs`

**Context:** iperf3's `-b` accepts strings like `"1M"` (1 Mbps), `"1K"` or `"1k"` (1 Kbps), `"1G"`, and plain integers (bps). Empty/zero means unlimited. The suffix is always in bits (lowercase `k`/`m`/`g` and uppercase all map to bits; iperf3 does not change unit based on case for `-b`).

- [ ] **Step 1: Create file with failing tests**

Create `src/common/bandwidth.rs`:

```rust
//! Parse iperf3-style bandwidth strings for `-b`.
//! Accepts integer bits-per-second with optional K/M/G suffix (case-
//! insensitive). Returns bps. "0" (or "") means unlimited.

pub fn parse(s: &str) -> Result<u64, String> {
    if s.is_empty() {
        return Ok(0);
    }
    let (num_part, mult): (&str, u64) = match s.as_bytes().last() {
        Some(b'k') | Some(b'K') => (&s[..s.len() - 1], 1_000),
        Some(b'm') | Some(b'M') => (&s[..s.len() - 1], 1_000_000),
        Some(b'g') | Some(b'G') => (&s[..s.len() - 1], 1_000_000_000),
        _ => (s, 1),
    };
    let n: u64 = num_part
        .parse()
        .map_err(|e| format!("invalid bandwidth '{}': {}", s, e))?;
    n.checked_mul(mult)
        .ok_or_else(|| format!("bandwidth '{}' overflows u64", s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_is_unlimited() {
        assert_eq!(parse("0").unwrap(), 0);
        assert_eq!(parse("").unwrap(), 0);
    }

    #[test]
    fn integer_bps_passthrough() {
        assert_eq!(parse("1000").unwrap(), 1_000);
    }

    #[test]
    fn k_suffix_is_thousand_bits() {
        assert_eq!(parse("1k").unwrap(), 1_000);
        assert_eq!(parse("250K").unwrap(), 250_000);
    }

    #[test]
    fn m_suffix_is_million_bits() {
        assert_eq!(parse("1M").unwrap(), 1_000_000);
        assert_eq!(parse("100m").unwrap(), 100_000_000);
    }

    #[test]
    fn g_suffix_is_billion_bits() {
        assert_eq!(parse("2G").unwrap(), 2_000_000_000);
    }

    #[test]
    fn invalid_input_errors() {
        assert!(parse("abc").is_err());
        assert!(parse("1.5M").is_err());
        assert!(parse("M").is_err());
    }

    #[test]
    fn overflow_errors() {
        assert!(parse("99999999999G").is_err());
    }
}
```

- [ ] **Step 2: Declare submodule**

Edit `src/common/mod.rs` — add `pub mod bandwidth;`.

- [ ] **Step 3: Run tests**

Run: `cargo test --lib bandwidth`
Expected: 7 tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/common/bandwidth.rs src/common/mod.rs
git commit -m "bandwidth: iperf3 -b string parser (K/M/G suffixes)"
```

---

## Task 6: `TransportKind` enum

**Files:**
- Modify: `src/common/mod.rs` (declare and re-export enum)
- Create: `src/common/transport_kind.rs`
- Test: `src/common/transport_kind.rs`

**Context:** Today `Config` has no notion of protocol — the tool is TCP-only. Adding a `transport: TransportKind` field pipes the protocol choice from CLI through `Config` and into the stream-start branches. Named `TransportKind` to avoid collision with the existing `Protocol` struct in `protocol.rs`.

- [ ] **Step 1: Create file with failing tests**

Create `src/common/transport_kind.rs`:

```rust
//! Which transport a rPerf3 test uses for its data streams. Control
//! channel is always TCP.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    Tcp,
    Udp,
}

impl Default for TransportKind {
    fn default() -> Self {
        Self::Tcp
    }
}

impl TransportKind {
    pub fn is_udp(self) -> bool {
        matches!(self, Self::Udp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_tcp() {
        assert_eq!(TransportKind::default(), TransportKind::Tcp);
    }

    #[test]
    fn is_udp_true_only_for_udp() {
        assert!(!TransportKind::Tcp.is_udp());
        assert!(TransportKind::Udp.is_udp());
    }
}
```

- [ ] **Step 2: Declare submodule**

Edit `src/common/mod.rs` — add `pub mod transport_kind;` and at the top, `pub use transport_kind::TransportKind;`.

- [ ] **Step 3: Run tests**

Run: `cargo test --lib transport_kind`
Expected: 2 tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/common/transport_kind.rs src/common/mod.rs
git commit -m "common: TransportKind enum (Tcp | Udp)"
```

---

## Task 7: Extend `Config` with transport + bandwidth

**Files:**
- Modify: `src/common/test.rs`
- Modify: `tests/self_test.rs` (existing `Config` constructors)
- Test: `src/common/test.rs`

**Context:** All call sites that build `Config` literally (not via `Config::with_host`) must get the new fields. The self-test's inline `Config { ... }` literals are the affected callers.

- [ ] **Step 1: Write the failing test**

Append to `src/common/test.rs` tests module:

```rust
#[test]
fn config_with_host_defaults_tcp_and_unlimited_bandwidth() {
    let cfg = Config::with_host("h");
    assert_eq!(cfg.transport, crate::common::TransportKind::Tcp);
    assert_eq!(cfg.bandwidth, 0);
}
```

- [ ] **Step 2: Run test to verify failure**

Run: `cargo test --lib test::tests::config_with_host_defaults_tcp_and_unlimited_bandwidth`
Expected: compilation error — missing `transport` field.

- [ ] **Step 3: Extend `Config`**

In `src/common/test.rs`, edit the `Config` struct and `with_host`:

```rust
use crate::common::TransportKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub time: u32,
    pub parallel: u32,
    pub len: u32,
    pub omit: u32,
    pub transport: TransportKind,
    /// Sender-side bandwidth limit in bits per second. 0 = unlimited.
    /// Applies to UDP by default (matching iperf3) and to TCP when
    /// explicitly set via `-b`.
    pub bandwidth: u64,
}

impl Config {
    #[allow(dead_code)]
    pub fn with_host(host: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            port: 5201,
            time: 10,
            parallel: 1,
            len: DEFAULT_TCP_LEN as u32,
            omit: 0,
            transport: TransportKind::Tcp,
            bandwidth: 0,
        }
    }
    // (host_port unchanged)
}
```

- [ ] **Step 4: Fix existing Config literal in tests**

Edit `src/common/test.rs` — in the existing `config_host_port_formats_correctly` test, add `transport: TransportKind::Tcp, bandwidth: 0,` to the struct literal.

Edit `tests/self_test.rs` — in both `Config { ... }` literals (the 1-second TCP test and the omit test), add `transport: rperf3::TransportKind::Tcp, bandwidth: 0,`. This requires `rperf3::TransportKind` to be exported.

- [ ] **Step 5: Re-export `TransportKind` from the crate root**

Edit `src/lib.rs` — change the `pub use` line to:

```rust
pub use common::test::Config;
pub use common::TransportKind;
```

- [ ] **Step 6: Run tests**

Run: `cargo test --lib test::tests && cargo test --test self_test --no-run`
Expected: unit tests pass; self-test compiles (not necessarily run).

- [ ] **Step 7: Commit**

```bash
git add src/common/test.rs src/lib.rs tests/self_test.rs
git commit -m "config: add transport kind and bandwidth; re-export TransportKind"
```

---

## Task 8: CLI flags `-u` and `-b`

**Files:**
- Modify: `src/cli.rs`
- Test: `src/cli.rs`

**Context:** `-u/--udp` toggles UDP mode. `-b/--bandwidth` takes a rate string (parsed via `common::bandwidth::parse`). When UDP is chosen and no bandwidth given, iperf3's default is 1 Mbps — we match. When TCP, default is 0 (unlimited).

- [ ] **Step 1: Write failing tests**

Append to `src/cli.rs` tests module:

```rust
#[test]
fn parses_udp_flag() {
    let cli = Cli::try_parse_from(["rperf3", "-c", "h", "-u"]).unwrap();
    assert!(cli.udp);
    match cli.into_mode() {
        Mode::Client(cfg) => {
            assert_eq!(cfg.transport, crate::common::TransportKind::Udp);
            // Default UDP bandwidth is 1 Mbps.
            assert_eq!(cfg.bandwidth, 1_000_000);
        }
        Mode::Server(_) => panic!("expected client"),
    }
}

#[test]
fn parses_bandwidth_flag() {
    let cli = Cli::try_parse_from(["rperf3", "-c", "h", "-u", "-b", "100M"]).unwrap();
    match cli.into_mode() {
        Mode::Client(cfg) => assert_eq!(cfg.bandwidth, 100_000_000),
        _ => panic!("expected client"),
    }
}

#[test]
fn bandwidth_zero_is_unlimited() {
    let cli = Cli::try_parse_from(["rperf3", "-c", "h", "-u", "-b", "0"]).unwrap();
    match cli.into_mode() {
        Mode::Client(cfg) => assert_eq!(cfg.bandwidth, 0),
        _ => panic!("expected client"),
    }
}

#[test]
fn tcp_default_bandwidth_is_zero() {
    let cli = Cli::try_parse_from(["rperf3", "-c", "h"]).unwrap();
    match cli.into_mode() {
        Mode::Client(cfg) => {
            assert_eq!(cfg.transport, crate::common::TransportKind::Tcp);
            assert_eq!(cfg.bandwidth, 0);
        }
        _ => panic!("expected client"),
    }
}

#[test]
fn invalid_bandwidth_errors_on_into_mode() {
    let cli = Cli::try_parse_from(["rperf3", "-c", "h", "-b", "garbage"]).unwrap();
    // into_mode should propagate the parse error (via panic or Result — we choose Result).
    assert!(cli.try_into_mode().is_err());
}
```

- [ ] **Step 2: Run failing tests**

Run: `cargo test --lib cli::tests`
Expected: compilation error — no `udp` field, no `try_into_mode`.

- [ ] **Step 3: Extend `Cli` and add `try_into_mode`**

Edit `src/cli.rs`:

```rust
// Add near the top of imports:
use crate::common::{bandwidth, TransportKind};

// Default UDP bandwidth: iperf3's default is 1 Mbps when -u is given without -b.
pub const DEFAULT_UDP_BANDWIDTH_BPS: u64 = 1_000_000;

// In Cli struct, add:
    /// Use UDP rather than TCP for the data streams.
    #[arg(short = 'u', long = "udp")]
    pub udp: bool,

    /// Target bandwidth for UDP (and TCP when set). Accepts integer bps,
    /// or K/M/G suffixes. `0` = unlimited. Default: `1M` when `-u`, else `0`.
    #[arg(short = 'b', long = "bandwidth")]
    pub bandwidth: Option<String>,
```

Replace `into_mode` with a fallible companion and keep a panicking wrapper for the binary entry point:

```rust
impl Cli {
    pub fn into_mode(self) -> Mode {
        self.try_into_mode().unwrap_or_else(|e| {
            eprintln!("{}", e);
            std::process::exit(2);
        })
    }

    pub fn try_into_mode(self) -> Result<Mode, String> {
        let transport = if self.udp { TransportKind::Udp } else { TransportKind::Tcp };
        let bandwidth = match self.bandwidth.as_deref() {
            Some(s) => bandwidth::parse(s)?,
            None if self.udp => DEFAULT_UDP_BANDWIDTH_BPS,
            None => 0,
        };

        let base = Config {
            host: self.host.clone().unwrap_or_else(|| DEFAULT_BIND.to_string()),
            port: self.port,
            time: self.time,
            parallel: self.parallel,
            len: self.len,
            omit: self.omit,
            transport,
            bandwidth,
        };
        Ok(if self.server { Mode::Server(base) } else { Mode::Client(base) })
    }
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test --lib cli::tests`
Expected: all CLI tests pass (new and existing).

- [ ] **Step 5: Commit**

```bash
git add src/cli.rs
git commit -m "cli: add -u/--udp and -b/--bandwidth flags"
```

---

## Task 9: Extend `ClientStreamReceipt` with UDP fields

**Files:**
- Modify: `src/common/test.rs`
- Test: `src/common/test.rs`

**Context:** The client-side per-stream receipt needs four new fields to carry UDP accounting back to the main loop. All default to zero. Existing tests that construct `ClientStreamReceipt` with struct literals (in `src/client.rs` tests) need the new fields too.

- [ ] **Step 1: Write failing test**

Append to `src/common/test.rs` tests:

```rust
#[test]
fn client_receipt_empty_zeros_udp_fields() {
    let r = ClientStreamReceipt::empty(1);
    assert_eq!(r.jitter_ms, 0.0);
    assert_eq!(r.lost, 0);
    assert_eq!(r.ooo, 0);
    assert_eq!(r.packets, 0);
}
```

- [ ] **Step 2: Run failing test**

Run: `cargo test --lib test::tests::client_receipt_empty_zeros_udp_fields`
Expected: compile error.

- [ ] **Step 3: Extend struct and `empty`**

Edit `ClientStreamReceipt` in `src/common/test.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClientStreamReceipt {
    pub stream_id: u32,
    pub bytes_sent: u64,
    pub bytes_omit: u64,
    pub first_send_at: Option<Instant>,
    pub first_measured_at: Option<Instant>,
    pub last_send_at: Option<Instant>,
    pub retransmits: u32,
    pub jitter_ms: f64,
    pub lost: u64,
    pub ooo: u64,
    pub packets: u64,
}
```

(Note `Eq` is removed from the derive because `f64` is not `Eq`. Replace with `PartialEq` only.)

Update `empty`:

```rust
impl ClientStreamReceipt {
    pub fn empty(stream_id: u32) -> Self {
        Self {
            stream_id,
            bytes_sent: 0,
            bytes_omit: 0,
            first_send_at: None,
            first_measured_at: None,
            last_send_at: None,
            retransmits: 0,
            jitter_ms: 0.0,
            lost: 0,
            ooo: 0,
            packets: 0,
        }
    }
    // ... (bytes_measured, elapsed, measured_elapsed unchanged)
}
```

- [ ] **Step 4: Fix existing struct literals**

In `src/client.rs` tests module, the `client_receipt_with` helper constructs a literal. Add the four new fields set to zero/0.0:

```rust
fn client_receipt_with(...) -> ClientStreamReceipt {
    ClientStreamReceipt {
        // ... existing fields
        retransmits: 0,
        jitter_ms: 0.0,
        lost: 0,
        ooo: 0,
        packets: 0,
    }
}
```

- [ ] **Step 5: Run tests**

Run: `cargo test --lib test::tests && cargo test --lib client::tests`
Expected: all pass.

- [ ] **Step 6: Commit**

```bash
git add src/common/test.rs src/client.rs
git commit -m "test: extend ClientStreamReceipt with UDP accounting fields"
```

---

## Task 10: Extend server `StreamReceipt` with UDP fields

**Files:**
- Modify: `src/server.rs`
- Test: `src/server.rs`

**Context:** Server-side receipt mirrors the client-side additions. Same `Eq`-removal caveat on `f64`.

- [ ] **Step 1: Write failing test**

Append to `src/server.rs` tests module:

```rust
#[test]
fn server_receipt_empty_zeros_udp_fields() {
    let r = StreamReceipt::empty();
    assert_eq!(r.jitter_ms, 0.0);
    assert_eq!(r.lost, 0);
    assert_eq!(r.ooo, 0);
    assert_eq!(r.packets, 0);
}
```

- [ ] **Step 2: Run failing test**

Run: `cargo test --lib server::tests::server_receipt_empty_zeros_udp_fields`
Expected: compile error.

- [ ] **Step 3: Extend struct + `empty`**

Edit `src/server.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StreamReceipt {
    pub bytes: u64,
    pub bytes_omit: u64,
    pub first_byte_at: Option<Instant>,
    pub first_measured_at: Option<Instant>,
    pub last_byte_at: Option<Instant>,
    pub jitter_ms: f64,
    pub lost: u64,
    pub ooo: u64,
    pub packets: u64,
}

impl StreamReceipt {
    pub fn empty() -> Self {
        Self {
            bytes: 0,
            bytes_omit: 0,
            first_byte_at: None,
            first_measured_at: None,
            last_byte_at: None,
            jitter_ms: 0.0,
            lost: 0,
            ooo: 0,
            packets: 0,
        }
    }
    // bytes_measured / elapsed / measured_elapsed unchanged.
}
```

- [ ] **Step 4: Fix existing `receipt_with` helper in `src/server.rs` tests**

The existing `fn receipt_with(bytes, first, last) -> StreamReceipt` literal needs the new fields. Add zero/0.0 defaults.

- [ ] **Step 5: Run tests**

Run: `cargo test --lib server::tests`
Expected: all pass.

- [ ] **Step 6: Commit**

```bash
git add src/server.rs
git commit -m "server: extend StreamReceipt with UDP accounting fields"
```

---

## Task 11: Client-side UDP stream sender (`Stream::start_udp`)

**Files:**
- Modify: `src/common/stream.rs`
- Test: `src/common/stream.rs`

**Context:** Spawns a thread that opens a connected UDP socket to the server, sends the cookie as the first datagram, then loops sending packets with the 16-byte header and a payload-filler up to `config.len` total bytes per packet, paced by `TokenBucket`. Stops on timer expiration; sends a sentinel datagram (seq=-1) before exit. Reports per-stream receipt via the existing `receipt_tx` channel.

The test here is a narrow integration test: we bind a real loopback `UdpSocket` on the receiving side, spawn `start_udp` pointing at it, and assert that (a) the first 37 bytes received match the cookie, (b) at least one data packet with a decodable header arrives, and (c) the sentinel packet arrives last.

- [ ] **Step 1: Write failing test**

Append to `src/common/stream.rs`:

```rust
#[test]
fn start_udp_sends_cookie_then_data_then_sentinel() {
    use crate::common::cookie::generate_cookie;
    use crate::common::test::{Config, Test};
    use crate::common::udp_header::{UdpHeader, UDP_HEADER_LEN};
    use crate::common::TransportKind;
    use std::net::UdpSocket;
    use std::time::Duration;

    let server = UdpSocket::bind("127.0.0.1:0").expect("bind server");
    server
        .set_read_timeout(Some(Duration::from_secs(3)))
        .expect("set read timeout");
    let port = server.local_addr().unwrap().port();

    let mut cfg = Config::with_host("127.0.0.1");
    cfg.port = port;
    cfg.time = 1;
    cfg.parallel = 1;
    cfg.len = 256; // small packets so we see several in 1 second
    cfg.transport = TransportKind::Udp;
    cfg.bandwidth = 1_000_000; // 1 Mbps

    let test = Test::new(cfg);
    let cookie = test.cookie;
    let host = test.config.host_port();

    Stream::start_udp(&test, host, cookie, 1);

    // First datagram: cookie (37 bytes).
    let mut buf = [0u8; 2048];
    let (n, _src) = server.recv_from(&mut buf).expect("recv cookie");
    assert_eq!(n, crate::common::cookie::COOKIE_LEN);
    assert_eq!(&buf[..n], &cookie);

    // Then at least one data datagram with a decodable header.
    let (n, _src) = server.recv_from(&mut buf).expect("recv data");
    assert!(n >= UDP_HEADER_LEN, "data packet too small: {}", n);
    let hdr = UdpHeader::decode(&buf[..UDP_HEADER_LEN]).expect("decode header");
    assert!(hdr.seq >= 0, "first data packet should not be sentinel");
    assert_eq!(n, 256, "packet length should match config.len");

    // Drain until we see the sentinel (or time out).
    let mut saw_sentinel = false;
    for _ in 0..200 {
        let (n, _src) = match server.recv_from(&mut buf) {
            Ok(r) => r,
            Err(_) => break,
        };
        if n < UDP_HEADER_LEN {
            continue;
        }
        let hdr = UdpHeader::decode(&buf[..UDP_HEADER_LEN]).unwrap();
        if hdr.is_sentinel() {
            saw_sentinel = true;
            break;
        }
    }
    assert!(saw_sentinel, "never saw sentinel packet");
}
```

- [ ] **Step 2: Run failing test**

Run: `cargo test --lib stream::tests::start_udp_sends_cookie_then_data_then_sentinel`
Expected: compile error — `start_udp` does not exist.

- [ ] **Step 3: Implement `start_udp`**

In `src/common/stream.rs`, add after `impl Stream { ... start ... }`:

```rust
use std::net::UdpSocket;
use super::pacing::TokenBucket;
use super::udp_header::{UdpHeader, UDP_HEADER_LEN};
use super::cookie::COOKIE_LEN;

impl Stream {
    pub fn start_udp(test: &Test, host: String, cookie: [u8; COOKIE_LEN], stream_id: u32) {
        let tx = test.tx_channel.clone();
        let receipt_tx = test.receipt_tx.clone();
        let len = test.config.len as usize;
        let duration = Duration::from_secs((test.config.time + test.config.omit) as u64);
        let omit = Duration::from_secs(test.config.omit as u64);
        let bandwidth = test.config.bandwidth;

        thread::spawn(move || {
            let mut receipt = ClientStreamReceipt::empty(stream_id);

            let socket = match UdpSocket::bind("0.0.0.0:0") {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("udp bind failed: {:?}", e);
                    emit_stream_finished(&receipt_tx, receipt, &tx);
                    return;
                }
            };
            if let Err(e) = socket.connect(&host) {
                eprintln!("udp connect failed: {:?}", e);
                emit_stream_finished(&receipt_tx, receipt, &tx);
                return;
            }

            // 1. Cookie as first datagram.
            if let Err(e) = socket.send(&cookie) {
                eprintln!("udp cookie send failed: {:?}", e);
                emit_stream_finished(&receipt_tx, receipt, &tx);
                return;
            }

            // 2. Data loop.
            if len < UDP_HEADER_LEN {
                eprintln!("packet len {} smaller than UDP header {}", len, UDP_HEADER_LEN);
                emit_stream_finished(&receipt_tx, receipt, &tx);
                return;
            }
            let mut packet = vec![1u8; len];
            let timer = Timer::new();
            let mut bucket = TokenBucket::new(bandwidth, Instant::now());
            let mut seq: i64 = 0;

            while !should_stop(&timer, duration) {
                let now = Instant::now();
                if let Some(wait) = bucket.wait(now) {
                    std::thread::sleep(wait);
                    continue;
                }
                let epoch = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default();
                let hdr = UdpHeader {
                    tv_sec: epoch.as_secs() as u32,
                    tv_usec: epoch.subsec_micros(),
                    seq,
                };
                if hdr.encode(&mut packet[..UDP_HEADER_LEN]).is_err() {
                    break;
                }
                match socket.send(&packet) {
                    Ok(n) if n > 0 => {
                        bucket.record(n as u64);
                        let sent_at = Instant::now();
                        if receipt.first_send_at.is_none() {
                            receipt.first_send_at = Some(sent_at);
                        }
                        receipt.last_send_at = Some(sent_at);
                        receipt.bytes_sent += n as u64;
                        receipt.packets += 1;
                        seq += 1;

                        let first = receipt.first_send_at.expect("set");
                        let in_omit = sent_at.duration_since(first) < omit;
                        if in_omit {
                            receipt.bytes_omit += n as u64;
                        } else if receipt.first_measured_at.is_none() {
                            receipt.first_measured_at = Some(sent_at);
                        }
                    }
                    Ok(_) => continue,
                    Err(e) => {
                        eprintln!("udp send error: {:?}", e);
                        break;
                    }
                }
            }

            // 3. Sentinel packet (seq = -1).
            let epoch = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            let sentinel = UdpHeader {
                tv_sec: epoch.as_secs() as u32,
                tv_usec: epoch.subsec_micros(),
                seq: -1,
            };
            let mut final_pkt = [0u8; UDP_HEADER_LEN];
            if sentinel.encode(&mut final_pkt).is_ok() {
                let _ = socket.send(&final_pkt);
            }

            emit_stream_finished(&receipt_tx, receipt, &tx);
        });
    }
}
```

Remember to import `Instant` (already imported) and `Duration` (already imported). The helper `emit_stream_finished` already exists; just reuse it.

- [ ] **Step 4: Run test**

Run: `cargo test --lib stream::tests::start_udp_sends_cookie_then_data_then_sentinel -- --nocapture`
Expected: pass. All three assertions (cookie received, data packet with decodable header, sentinel observed).

- [ ] **Step 5: Commit**

```bash
git add src/common/stream.rs
git commit -m "stream: Stream::start_udp — connected UDP sender with pacing and sentinel"
```

---

## Task 12: Server-side UDP session (bind + cookie demux + receiver loop)

**Files:**
- Create: `src/common/udp_session.rs`
- Modify: `src/common/mod.rs`
- Test: `src/common/udp_session.rs`

**Context:** One UDP socket bound on the test's TCP control port. Clients send cookie as first datagram per stream; server reads N cookies via `recv_from`, records each source `SocketAddr` as a stream key. Then loops reading data datagrams, demuxing by source addr, updating per-stream `StreamReceipt`. Stops when it has seen a sentinel from every stream OR the caller signals (via an `AtomicBool`) that TestEnd has arrived on the control channel.

This file is new; it does NOT depend on `server.rs`'s types directly. Server-side `StreamReceipt` is used as the output, so `udp_session.rs` imports it.

- [ ] **Step 1: Create file with initial types and failing test**

Create `src/common/udp_session.rs`:

```rust
//! Server-side UDP session: bind one socket on the test port, read one
//! cookie-bearing datagram per expected stream, then demux incoming
//! datagrams by source address into per-stream receipts.

use std::collections::HashMap;
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::cookie::COOKIE_LEN;
use super::jitter;
use super::udp_header::{UdpHeader, UDP_HEADER_LEN};
use crate::server::StreamReceipt;

/// Bind a blocking UDP socket on `addr`. Applies the handshake timeout
/// so a vanished client during cookie exchange does not hang the server.
pub fn bind_udp(addr: &str, handshake_timeout: Option<Duration>) -> io::Result<UdpSocket> {
    let socket = UdpSocket::bind(addr)?;
    socket.set_read_timeout(handshake_timeout)?;
    Ok(socket)
}

/// Accept `n` UDP "streams" by reading the first N datagrams and
/// verifying each one matches the expected cookie. Returns the source
/// `SocketAddr` of each accepted stream (ordering = arrival order).
/// After this call, the caller should clear the socket's read timeout
/// for the actual test window.
pub fn accept_udp_streams(
    socket: &UdpSocket,
    expected_cookie: &[u8; COOKIE_LEN],
    n: u32,
) -> io::Result<Vec<SocketAddr>> {
    let mut addrs = Vec::with_capacity(n as usize);
    let mut buf = [0u8; COOKIE_LEN + 64];
    for i in 0..n {
        let (len, src) = socket.recv_from(&mut buf)?;
        if len != COOKIE_LEN || &buf[..len] != expected_cookie {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("udp stream {} cookie mismatch or wrong length ({})", i, len),
            ));
        }
        addrs.push(src);
    }
    Ok(addrs)
}

/// Per-source tracking state used during the receive loop.
struct StreamState {
    receipt: StreamReceipt,
    max_seq: i64,
    last_transit_ms: Option<f64>,
    saw_sentinel: bool,
}

impl StreamState {
    fn new() -> Self {
        Self {
            receipt: StreamReceipt::empty(),
            max_seq: -1,
            last_transit_ms: None,
            saw_sentinel: false,
        }
    }
}

/// Drain UDP datagrams into per-stream receipts until either every
/// stream has signalled its sentinel OR `stop_flag` is set. Packets from
/// unknown source addresses are counted against no stream and dropped.
pub fn run_udp_receiver(
    socket: UdpSocket,
    stream_order: Vec<SocketAddr>,
    omit: Duration,
    stop_flag: Arc<AtomicBool>,
) -> Vec<StreamReceipt> {
    // Short read deadline so we can poll stop_flag without busy-looping
    // when no packets are arriving.
    let _ = socket.set_read_timeout(Some(Duration::from_millis(100)));

    let mut states: HashMap<SocketAddr, StreamState> = stream_order
        .iter()
        .map(|a| (*a, StreamState::new()))
        .collect();
    let mut buf = [0u8; 65_536];

    loop {
        if stop_flag.load(Ordering::Relaxed) && states.values().all(|s| s.saw_sentinel) {
            break;
        }
        if stop_flag.load(Ordering::Relaxed) {
            // Grace period: 200ms after TestEnd to drain in-flight packets.
            let _ = socket.set_read_timeout(Some(Duration::from_millis(50)));
        }

        match socket.recv_from(&mut buf) {
            Ok((len, src)) => {
                if len < UDP_HEADER_LEN {
                    continue;
                }
                let Some(state) = states.get_mut(&src) else {
                    continue; // unknown source
                };
                let hdr = match UdpHeader::decode(&buf[..UDP_HEADER_LEN]) {
                    Ok(h) => h,
                    Err(_) => continue,
                };
                let now = Instant::now();

                if hdr.is_sentinel() {
                    state.saw_sentinel = true;
                    // Don't count sentinels toward byte/packet totals —
                    // iperf3 doesn't either.
                    continue;
                }

                if state.receipt.first_byte_at.is_none() {
                    state.receipt.first_byte_at = Some(now);
                }
                state.receipt.last_byte_at = Some(now);
                state.receipt.bytes += len as u64;
                state.receipt.packets += 1;

                let first = state.receipt.first_byte_at.unwrap();
                let in_omit = now.duration_since(first) < omit;
                if in_omit {
                    state.receipt.bytes_omit += len as u64;
                } else if state.receipt.first_measured_at.is_none() {
                    state.receipt.first_measured_at = Some(now);
                }

                // Loss / OOO / jitter.
                if hdr.seq <= state.max_seq {
                    state.receipt.ooo += 1;
                } else {
                    if hdr.seq > state.max_seq + 1 {
                        let gap = (hdr.seq - state.max_seq - 1) as u64;
                        state.receipt.lost = state.receipt.lost.saturating_add(gap);
                    }
                    state.max_seq = hdr.seq;
                }

                let recv_ms = now
                    .duration_since(state.receipt.first_byte_at.unwrap())
                    .as_secs_f64()
                    * 1_000.0;
                let send_ms = (hdr.tv_sec as f64) * 1_000.0 + (hdr.tv_usec as f64) / 1_000.0;
                let transit = recv_ms - send_ms;
                if let Some(prev) = state.last_transit_ms {
                    let d = transit - prev;
                    state.receipt.jitter_ms = jitter::update(state.receipt.jitter_ms, d);
                }
                state.last_transit_ms = Some(transit);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut => {
                if stop_flag.load(Ordering::Relaxed) {
                    // After the grace deadline, we give up even if not every sentinel arrived.
                    break;
                }
                continue;
            }
            Err(_) => break,
        }
    }

    stream_order
        .into_iter()
        .map(|addr| states.remove(&addr).map(|s| s.receipt).unwrap_or_else(StreamReceipt::empty))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::cookie::generate_cookie;
    use std::thread;

    #[test]
    fn bind_udp_returns_socket_bound_to_addr() {
        let socket = bind_udp("127.0.0.1:0", None).expect("bind");
        assert!(socket.local_addr().is_ok());
    }

    #[test]
    fn accept_udp_streams_matches_cookie() {
        let server = bind_udp("127.0.0.1:0", Some(Duration::from_secs(2))).expect("bind");
        let cookie = generate_cookie();
        let addr = server.local_addr().unwrap();

        let client_cookie = cookie;
        let client = thread::spawn(move || {
            let sock = UdpSocket::bind("127.0.0.1:0").expect("client bind");
            sock.connect(addr).expect("client connect");
            sock.send(&client_cookie).expect("client send");
            sock
        });

        let addrs = accept_udp_streams(&server, &cookie, 1).expect("accept");
        assert_eq!(addrs.len(), 1);
        let _ = client.join();
    }

    #[test]
    fn accept_udp_streams_rejects_bad_cookie() {
        let server = bind_udp("127.0.0.1:0", Some(Duration::from_secs(2))).expect("bind");
        let good = generate_cookie();
        let mut bad = good;
        bad[0] = bad[0].wrapping_add(1);
        let addr = server.local_addr().unwrap();

        let client = thread::spawn(move || {
            let sock = UdpSocket::bind("127.0.0.1:0").expect("client bind");
            sock.connect(addr).expect("connect");
            sock.send(&bad).expect("send");
        });

        let err = accept_udp_streams(&server, &good, 1).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        let _ = client.join();
    }

    #[test]
    fn run_udp_receiver_accounts_packets_and_stops_on_sentinel() {
        let server = bind_udp("127.0.0.1:0", Some(Duration::from_secs(3))).expect("bind");
        let cookie = generate_cookie();
        let server_addr = server.local_addr().unwrap();

        // Single UDP stream sender.
        let sender = thread::spawn(move || {
            let sock = UdpSocket::bind("127.0.0.1:0").expect("client bind");
            sock.connect(server_addr).expect("client connect");
            sock.send(&cookie).expect("send cookie");

            // Send a few data packets.
            for seq in 0i64..5 {
                let mut buf = [0u8; 64];
                UdpHeader { tv_sec: 0, tv_usec: 0, seq }
                    .encode(&mut buf[..UDP_HEADER_LEN])
                    .unwrap();
                sock.send(&buf).expect("send data");
                thread::sleep(Duration::from_millis(5));
            }
            // Sentinel.
            let mut buf = [0u8; UDP_HEADER_LEN];
            UdpHeader { tv_sec: 0, tv_usec: 0, seq: -1 }.encode(&mut buf).unwrap();
            sock.send(&buf).expect("send sentinel");
        });

        let stream_addrs = accept_udp_streams(&server, &cookie, 1).expect("accept");
        assert_eq!(stream_addrs.len(), 1);

        let stop = Arc::new(AtomicBool::new(true)); // tell receiver to stop after sentinel
        let receipts = run_udp_receiver(server, stream_addrs, Duration::ZERO, stop);
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].packets, 5);
        assert_eq!(receipts[0].bytes, 5 * 64);
        assert_eq!(receipts[0].lost, 0);
        assert_eq!(receipts[0].ooo, 0);

        let _ = sender.join();
    }
}
```

- [ ] **Step 2: Declare submodule**

Edit `src/common/mod.rs` — add `pub mod udp_session;`.

- [ ] **Step 3: Run tests**

Run: `cargo test --lib udp_session::tests -- --nocapture`
Expected: 4 tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/common/udp_session.rs src/common/mod.rs
git commit -m "udp_session: server bind + cookie demux + receiver with jitter/loss/OOO"
```

---

## Task 13: Wire UDP into server state machine

**Files:**
- Modify: `src/server.rs`
- Test: `src/server.rs`

**Context:** `serve_one` currently always runs the TCP data path. With `opts.udp == true`, it instead runs the UDP path: bind UDP on the listener's port, accept N UDP streams via cookie, spawn a receiver thread that runs `run_udp_receiver`, wait for control-channel TestEnd, signal the receiver to stop, join, build Results with jitter/lost/packets populated.

The existing TCP path stays intact for `opts.udp == false`.

- [ ] **Step 1: Write failing test**

Append to `src/server.rs` tests module:

```rust
#[test]
fn build_server_results_populates_udp_fields_from_receipts() {
    let r = StreamReceipt {
        bytes: 1000,
        bytes_omit: 0,
        first_byte_at: None,
        first_measured_at: None,
        last_byte_at: None,
        jitter_ms: 2.5,
        lost: 7,
        ooo: 1,
        packets: 50,
    };
    let results = build_server_results(&[r], &CpuUsage::ZERO);
    assert_eq!(results.streams[0].jitter, 2.5);
    assert_eq!(results.streams[0].errors, 7);
    assert_eq!(results.streams[0].packets, 50);
}
```

- [ ] **Step 2: Run failing test**

Run: `cargo test --lib server::tests::build_server_results_populates_udp_fields_from_receipts`
Expected: fail (old `build_server_results` hardcodes jitter=0.0, errors=0, packets=0).

- [ ] **Step 3: Update `build_server_results`**

In `src/server.rs`:

```rust
pub fn build_server_results(receipts: &[StreamReceipt], cpu: &CpuUsage) -> Results {
    let streams = receipts
        .iter()
        .enumerate()
        .map(|(i, r)| StreamResults {
            id: (i + 1) as u32,
            bytes: r.bytes,
            retransmits: 0,
            jitter: r.jitter_ms,
            errors: r.lost,
            packets: r.packets,
        })
        .collect();

    Results {
        cpu_util_total: cpu.total_pct,
        cpu_util_user: cpu.user_pct,
        cpu_util_system: cpu.system_pct,
        sender_has_retransmits: 0,
        streams,
    }
}
```

- [ ] **Step 4: Add UDP branch in `serve_one`**

Top of `src/server.rs`, add imports:

```rust
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use crate::common::udp_session::{accept_udp_streams, bind_udp, run_udp_receiver};
```

Refactor `serve_one` — after `negotiate_options`, branch on `opts.udp`:

```rust
fn serve_one(listener: &TcpListener, handshake_timeout: Option<Duration>) -> io::Result<u64> {
    let (mut control, cookie) = accept_control(listener, handshake_timeout)?;
    let cpu_start = cpu::sample();
    println!("control connection cookie: {}", cookie_display(&cookie));

    let opts = negotiate_options(&mut control)?;
    println!("negotiated options: {:?}", opts);

    let receipts = if opts.udp {
        run_udp_branch(&mut control, listener, &cookie, &opts, handshake_timeout)?
    } else {
        run_tcp_branch(&mut control, listener, &cookie, &opts, handshake_timeout)?
    };

    let total_bytes: u64 = receipts.iter().map(|r| r.bytes).sum();
    let measured_bytes: u64 = receipts.iter().map(|r| r.bytes_measured()).sum();
    let raw = measured_duration(&receipts);
    let steady = measured_steady_state(&receipts);
    println!(
        "total bytes received: {} ({} post-omit) across {} stream(s) over {:.6}s (steady-state {:.6}s)",
        total_bytes, measured_bytes, receipts.len(),
        raw.as_secs_f64(), steady.as_secs_f64(),
    );

    control.transfer.set_read_timeout(handshake_timeout)?;
    let wall = if !steady.is_zero() { steady } else if !raw.is_zero() { raw } else { Duration::ZERO };
    let cpu_end = cpu::sample();
    let cpu_usage = cpu::usage(&cpu_start, &cpu_end, wall);
    if cpu_usage.total_pct > 0.0 {
        println!(
            "[SERVER] CPU: {:.2}% total ({:.2}% user, {:.2}% system)",
            cpu_usage.total_pct, cpu_usage.user_pct, cpu_usage.system_pct,
        );
    }

    exchange_results(&mut control, &receipts, &cpu_usage)?;
    println!("results exchanged");
    finalize_test(&mut control)?;

    let (summary_bytes, summary_duration) = if opts.omit > 0 && !steady.is_zero() {
        (measured_bytes, steady)
    } else if !raw.is_zero() {
        (total_bytes, raw)
    } else {
        (total_bytes, Duration::from_secs(opts.time as u64))
    };
    print_summary(&format_summary(summary_bytes, summary_duration, receipts.len()));
    Ok(total_bytes)
}

fn run_tcp_branch(
    control: &mut Protocol,
    listener: &TcpListener,
    cookie: &[u8; COOKIE_LEN],
    opts: &ClientOptions,
    handshake_timeout: Option<Duration>,
) -> io::Result<Vec<StreamReceipt>> {
    let streams = accept_streams(control, listener, cookie, opts.parallel, handshake_timeout)?;
    println!("accepted {} data stream(s)", streams.len());
    send_test_start_running(control)?;
    println!("test is running");

    let run_timeout = handshake_timeout
        .map(|h| h + Duration::from_secs((opts.time + opts.omit) as u64));
    control.transfer.set_read_timeout(run_timeout)?;

    let omit_window = Duration::from_secs(opts.omit as u64);
    let handles = spawn_stream_readers(streams, omit_window);
    wait_for_test_end(control)?;
    Ok(join_stream_totals(handles))
}

fn run_udp_branch(
    control: &mut Protocol,
    listener: &TcpListener,
    cookie: &[u8; COOKIE_LEN],
    opts: &ClientOptions,
    handshake_timeout: Option<Duration>,
) -> io::Result<Vec<StreamReceipt>> {
    // Bind UDP on the same port TCP is listening on. TCP and UDP are
    // separate protocols — binding the same port for both is fine.
    let local = listener.local_addr()?;
    let bind_str = format!("{}:{}", local.ip(), local.port());
    let udp = bind_udp(&bind_str, handshake_timeout)?;

    // Tell the client to open its data streams, then read cookies.
    send_control_byte(control.transfer.as_mut(), Message::CreateStreams)?;
    let addrs = accept_udp_streams(&udp, cookie, opts.parallel)?;
    println!("accepted {} udp stream(s)", addrs.len());

    // Clear the handshake deadline for the actual test window; the
    // receiver loop sets its own short poll timeout.
    udp.set_read_timeout(None)?;

    send_test_start_running(control)?;

    let run_timeout = handshake_timeout
        .map(|h| h + Duration::from_secs((opts.time + opts.omit) as u64));
    control.transfer.set_read_timeout(run_timeout)?;

    // Spawn receiver.
    let stop = Arc::new(AtomicBool::new(false));
    let receiver_stop = stop.clone();
    let omit_window = Duration::from_secs(opts.omit as u64);
    let handle = std::thread::spawn(move || {
        run_udp_receiver(udp, addrs, omit_window, receiver_stop)
    });

    // Wait for TestEnd on the control channel, then signal the receiver.
    wait_for_test_end(control)?;
    stop.store(true, std::sync::atomic::Ordering::Relaxed);

    let receipts = handle.join().unwrap_or_default();
    Ok(receipts)
}
```

- [ ] **Step 5: Run tests**

Run: `cargo test --lib server::tests`
Expected: all pass (old TCP tests + new results-population test).

- [ ] **Step 6: Commit**

```bash
git add src/server.rs
git commit -m "server: wire UDP branch through serve_one; populate jitter/lost/packets in Results"
```

---

## Task 14: Wire UDP into client state machine

**Files:**
- Modify: `src/client.rs`
- Test: `src/client.rs`

**Context:** `create_streams` unconditionally calls `Stream::start`. With `test.config.transport == Udp`, it must call `Stream::start_udp` instead. `send_options` must set `options.udp` and `options.bandwidth` from config. `build_client_results` must forward UDP fields from receipts.

- [ ] **Step 1: Write failing test**

Append to `src/client.rs` tests module:

```rust
#[test]
fn build_client_results_populates_udp_fields() {
    let mut r = ClientStreamReceipt::empty(1);
    r.bytes_sent = 2048;
    r.jitter_ms = 0.75;
    r.lost = 3;
    r.packets = 40;
    let out = build_client_results(&[r], &CpuUsage::ZERO);
    assert_eq!(out.streams[0].jitter, 0.75);
    assert_eq!(out.streams[0].errors, 3);
    assert_eq!(out.streams[0].packets, 40);
}
```

- [ ] **Step 2: Run failing test**

Run: `cargo test --lib client::tests::build_client_results_populates_udp_fields`
Expected: fail (old builder hardcodes 0/0/0).

- [ ] **Step 3: Update `build_client_results`**

In `src/client.rs`:

```rust
pub fn build_client_results(
    receipts: &[crate::common::test::ClientStreamReceipt],
    cpu: &CpuUsage,
) -> Results {
    let any_retrans = receipts.iter().any(|r| r.retransmits > 0);
    let streams = receipts
        .iter()
        .map(|r| StreamResults {
            id: r.stream_id,
            bytes: r.bytes_sent,
            retransmits: r.retransmits as i64,
            jitter: r.jitter_ms,
            errors: r.lost,
            packets: r.packets,
        })
        .collect();

    Results {
        cpu_util_total: cpu.total_pct,
        cpu_util_user: cpu.user_pct,
        cpu_util_system: cpu.system_pct,
        sender_has_retransmits: if any_retrans { 1 } else { 0 },
        streams,
    }
}
```

- [ ] **Step 4: Update `send_options` and `create_streams`**

In `src/client.rs`:

```rust
fn send_options(test: &mut Test) {
    let Some(ref mut protocol) = test.control_channel else { return; };

    let mut options =
        ClientOptions::tcp_defaults(test.config.time, test.config.parallel, test.config.len);
    options.omit = test.config.omit;
    options.udp = test.config.transport.is_udp();
    options.bandwidth = test.config.bandwidth;
    // When UDP, iperf3 expects tcp=false.
    if options.udp {
        options.tcp = false;
    }
    match send_framed_json(protocol.transfer.as_mut(), &options) {
        Ok(()) => println!("Options sent."),
        Err(e) => eprintln!("failed to send options: {:?}", e),
    }
}

fn create_streams(test: &Test) {
    let host = test.config.host_port();
    let n = test.config.parallel.max(1);
    for stream_id in 1..=n {
        if test.config.transport.is_udp() {
            Stream::start_udp(test, host.clone(), test.cookie, stream_id);
        } else {
            Stream::start(test, host.clone(), test.cookie, stream_id);
        }
    }
}
```

- [ ] **Step 5: Run tests**

Run: `cargo test --lib client::tests`
Expected: all pass.

- [ ] **Step 6: Commit**

```bash
git add src/client.rs
git commit -m "client: UDP-aware send_options + create_streams; populate UDP Results fields"
```

---

## Task 15: End-to-end UDP self-test

**Files:**
- Modify: `tests/self_test.rs`

**Context:** Mirrors the existing `self_test_loopback_tcp_short_run` but runs UDP. Because the server binds UDP on the same port as the TCP listener, we can reuse the existing `run_server_on` scaffold — no API changes needed. The test asserts `total_bytes > 0` (proxy for "data flowed") and exits cleanly.

- [ ] **Step 1: Add failing test**

Append to `tests/self_test.rs`:

```rust
#[test]
fn self_test_loopback_udp_short_run() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    let port = listener.local_addr().expect("local_addr").port();

    let server = thread::spawn(move || rperf3::run_server_on(listener));

    let client_cfg = rperf3::Config {
        host: "127.0.0.1".to_string(),
        port,
        time: 1,
        parallel: 1,
        len: 1400, // typical MTU-ish UDP payload
        omit: 0,
        transport: rperf3::TransportKind::Udp,
        bandwidth: 10_000_000, // 10 Mbps
    };

    thread::sleep(Duration::from_millis(50));
    rperf3::run_client(client_cfg);

    let total_bytes = server
        .join()
        .expect("server thread panicked")
        .expect("server returned error");

    // 10 Mbps for 1s ≈ 1.25 MB. Very conservative threshold.
    assert!(total_bytes > 100_000, "server saw {} bytes", total_bytes);
}
```

- [ ] **Step 2: Run test**

Run: `cargo test --test self_test self_test_loopback_udp_short_run -- --nocapture`
Expected: pass; output shows UDP stream handshake + bytes flowed.

- [ ] **Step 3: Run entire self-test suite to confirm no regressions**

Run: `cargo test --test self_test`
Expected: all three tests pass (TCP, omit, timeout) + new UDP test.

- [ ] **Step 4: Commit**

```bash
git add tests/self_test.rs
git commit -m "self_test: UDP loopback short run"
```

---

## Task 16: Cross-iperf3 interop test (data path)

**Files:**
- Create: `tests/cross_interop_udp.rs`

**Context:** Spawns the real `iperf3` binary. Skips with a printed message when `iperf3` is not on PATH (so contributors without it aren't blocked). Runs both directions: rPerf3-client↔iperf3-server and iperf3-client↔rPerf3-server, in UDP mode at 10 Mbps for 1 second. Asserts exit code 0 for the external side and that our side reports data flowed.

- [ ] **Step 1: Create the test file**

Create `tests/cross_interop_udp.rs`:

```rust
//! Cross-iperf3 UDP interop. Skips when the `iperf3` binary is not on
//! PATH; otherwise runs rPerf3↔iperf3 in both directions.

use std::io::Write;
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

fn iperf3_available() -> bool {
    Command::new("iperf3")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[test]
fn rperf3_client_talks_to_iperf3_server_udp() {
    if !iperf3_available() {
        println!("skipping cross-interop: iperf3 not on PATH");
        return;
    }

    // Start iperf3 server on an ephemeral port (we pick the port via
    // bind-and-close, accepting a small race window).
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let mut iperf3_server = Command::new("iperf3")
        .args(["-s", "-p", &port.to_string(), "-1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn iperf3 server");

    thread::sleep(Duration::from_millis(200));

    let client_cfg = rperf3::Config {
        host: "127.0.0.1".into(),
        port,
        time: 1,
        parallel: 1,
        len: 1400,
        omit: 0,
        transport: rperf3::TransportKind::Udp,
        bandwidth: 10_000_000,
    };
    rperf3::run_client(client_cfg);

    let output = iperf3_server.wait_with_output().expect("wait iperf3");
    assert!(output.status.success(), "iperf3 server failed: {:?}", output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Mbits/sec") || stdout.contains("bits/sec"),
            "iperf3 output missing throughput line: {}", stdout);
}

#[test]
fn iperf3_client_talks_to_rperf3_server_udp() {
    if !iperf3_available() {
        println!("skipping cross-interop: iperf3 not on PATH");
        return;
    }

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();

    let server = thread::spawn(move || rperf3::run_server_on(listener));
    thread::sleep(Duration::from_millis(200));

    let mut iperf3_client = Command::new("iperf3")
        .args([
            "-c", "127.0.0.1",
            "-p", &port.to_string(),
            "-u",
            "-b", "10M",
            "-t", "1",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn iperf3 client");

    let output = iperf3_client.wait_with_output().expect("wait iperf3");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(),
            "iperf3 client failed. stdout: {}\nstderr: {}", stdout, stderr);

    let total_bytes = server.join().expect("server panic").expect("server err");
    assert!(total_bytes > 100_000, "server saw only {} bytes", total_bytes);
}
```

- [ ] **Step 2: Run**

Run: `cargo test --test cross_interop_udp -- --nocapture`

Expected:
- If `iperf3` is installed: two tests pass.
- If not: two tests print "skipping cross-interop: iperf3 not on PATH" and pass.

- [ ] **Step 3: Commit**

```bash
git add tests/cross_interop_udp.rs
git commit -m "tests: cross-iperf3 UDP interop (both directions)"
```

---

## Task 17: Update README

**Files:**
- Modify: `README.md`

**Context:** Move UDP from "Out of scope" into "What works today." Document the new flags. Add a UDP example under "Test against itself" and "Test against real iPerf3".

- [ ] **Step 1: Update "What works today"**

In `README.md`, under "What works today", add a new bullet after the existing ones:

```
- **UDP data path (`-u`).** Full iPerf3-compatible UDP: 16-byte
  datagram header, RFC 1889 jitter EWMA, lost-packet and out-of-order
  accounting, sender-side bandwidth pacing (`-b`, default 1 Mbps for
  UDP), and an end-of-test sentinel packet. Works bidirectionally
  against real iPerf3.
```

- [ ] **Step 2: Update "Out of scope"**

Delete the "- UDP data path (...)" line from the "Out of scope" section.

- [ ] **Step 3: Add `-u` and `-b` to the Usage block**

In the Usage code block, add after `-l, --len`:

```
  -u, --udp                  Use UDP instead of TCP for data streams
  -b, --bandwidth <RATE>     Target bandwidth for UDP sender (e.g. 100M, 1G, 0 = unlimited)
```

- [ ] **Step 4: Add a UDP example under "Test against itself"**

```
# UDP at 100 Mbps for 3 seconds
cargo run --release -- -c 127.0.0.1 -p 5202 -u -b 100M -t 3
```

- [ ] **Step 5: Add a UDP example under "Test against real iPerf3"**

```
# rPerf3 client -> iperf3 server (UDP)
iperf3 -s -p 5202
cargo run --release -- -c 127.0.0.1 -p 5202 -u -b 50M -t 3

# iperf3 client -> rPerf3 server (UDP)
cargo run --release -- -s -p 5202
iperf3 -c 127.0.0.1 -p 5202 -u -b 50M -t 3
```

- [ ] **Step 6: Commit**

```bash
git add README.md
git commit -m "docs: document UDP support in README"
```

---

## Task 18: Full test run, clippy, and final commit

**Context:** Confirm the whole suite is green after the UDP work lands. This is the verification gate before declaring the sub-project done.

- [ ] **Step 1: Full unit + integration test suite**

Run: `cargo test --all-targets`
Expected: all tests pass.

- [ ] **Step 2: Clippy with warnings-as-errors**

Run: `cargo clippy --all-targets -- -D warnings`
Expected: clean. Fix any warnings that surface; commit fixes as `"chore: clippy cleanup for udp sub-project"`.

- [ ] **Step 3: Release build smoke test**

Run: `cargo build --release`
Expected: builds cleanly.

- [ ] **Step 4: (Optional, if `iperf3` present) run cross-interop test explicitly**

Run: `cargo test --test cross_interop_udp -- --nocapture`
Expected: both tests pass or skip gracefully.

- [ ] **Step 5: Nothing to commit unless clippy surfaced fixes.**

---

## Verification Summary

When this plan is complete:

1. `cargo test --all-targets` is green.
2. `cargo clippy --all-targets -- -D warnings` is clean.
3. `rperf3 -s -p 5202` followed by `rperf3 -c 127.0.0.1 -p 5202 -u -b 50M -t 3` reports non-zero throughput, non-zero packets, near-zero loss/OOO on loopback.
4. `iperf3 -s -p 5202` + `rperf3 -c 127.0.0.1 -p 5202 -u -b 50M -t 3` interoperates (both sides report the same rough throughput).
5. `rperf3 -s -p 5202` + `iperf3 -c 127.0.0.1 -p 5202 -u -b 50M -t 3` interoperates.
6. README documents `-u` and `-b`; "Out of scope" no longer lists UDP.

When those are all true, sub-project #1 is done. The next sub-project (reverse + bidirectional) can begin per the roadmap at [docs/superpowers/specs/2026-04-18-iperf3-parity-roadmap.md](../specs/2026-04-18-iperf3-parity-roadmap.md).
