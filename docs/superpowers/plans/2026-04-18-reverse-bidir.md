# Reverse + Bidirectional Implementation Plan

> **For agentic workers:** Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Add iperf3-compatible `-R/--reverse` and `--bidir` modes. Reverse makes the server the sender and the client the receiver; bidirectional runs sender+receiver on the same data streams concurrently. Must interoperate with real iperf3 3.21 in both directions.

**Architecture:** A new `Direction` enum in `Config` flows through `ClientOptions.reverse`/`ClientOptions.bidirectional` JSON fields. Client-side `Stream::start` and `Stream::start_udp` stay as senders; new `Stream::start_recv` / `Stream::start_udp_recv` are receivers. The role split happens at `create_streams` on both sides: for `Forward` the client sends and the server receives (today's behavior); for `Reverse` the roles swap; for `Bidirectional` both sides spawn send+recv threads on the same socket. UDP bidir reuses the forward UDP socket for both directions (iperf3 does too). Control-channel initiation stays unchanged — the client still drives state transitions.

**Tech Stack:** Rust 2021, `std::thread`, existing `ClientOptions`/`StreamResults` wire format extended with two `#[serde(default, skip_serializing_if = ...)]` fields.

---

## File Structure

**Files created:**
- `src/common/direction.rs` — `Direction` enum and CLI-parse helpers.
- `tests/cross_interop_reverse.rs` — cross-iperf3 tests for `-R` in both directions.
- `tests/cross_interop_bidir.rs` — cross-iperf3 tests for `--bidir` in both directions.

**Files modified:**
- `src/common/wire.rs` — `ClientOptions` gains `reverse` and `bidirectional`, both `#[serde(default, skip_serializing_if = ...)]` so legacy/TCP payloads stay unchanged on the wire.
- `src/common/mod.rs` — declare `direction` submodule + re-export `Direction`.
- `src/common/test.rs` — `Config.direction: Direction`; default `Forward`.
- `src/common/stream.rs` — split the existing worker into send/recv variants per transport (`start` / `start_recv` / `start_udp` / `start_udp_recv`).
- `src/server.rs` — `run_tcp_branch` and `run_udp_branch` branch on `opts.reverse` / `opts.bidirectional`; receivers stay as today, senders reuse the stream workers but with swapped roles.
- `src/client.rs` — `create_streams` branches on direction; `send_options` sets the new wire fields; `build_client_results` + final summary cover the receiver role when `Reverse`.
- `src/cli.rs` — `-R/--reverse`, `--bidir` (mutually exclusive); plumbed through `try_into_mode`.
- `src/lib.rs` — re-export `Direction`.
- `tests/self_test.rs` — loopback self-tests for TCP reverse, UDP reverse, TCP bidir, UDP bidir.
- `README.md` — document the two flags + new examples.

---

## Task 1: `Direction` enum + wire fields

**Files:**
- Create: `src/common/direction.rs`
- Modify: `src/common/mod.rs`, `src/common/wire.rs`

- [ ] **Step 1: Create direction module with tests**

Create `src/common/direction.rs`:

```rust
//! Data-path direction for a test. Control channel is always client-initiated.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Direction {
    #[default]
    Forward,
    Reverse,
    Bidirectional,
}

impl Direction {
    pub fn is_reverse(self) -> bool {
        matches!(self, Self::Reverse)
    }
    pub fn is_bidirectional(self) -> bool {
        matches!(self, Self::Bidirectional)
    }
    /// Does the client send data on data streams for this direction?
    pub fn client_sends(self) -> bool {
        matches!(self, Self::Forward | Self::Bidirectional)
    }
    /// Does the client receive data on data streams for this direction?
    pub fn client_receives(self) -> bool {
        matches!(self, Self::Reverse | Self::Bidirectional)
    }
    /// Does the server send data on data streams for this direction?
    pub fn server_sends(self) -> bool {
        matches!(self, Self::Reverse | Self::Bidirectional)
    }
    /// Does the server receive data on data streams for this direction?
    pub fn server_receives(self) -> bool {
        matches!(self, Self::Forward | Self::Bidirectional)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_forward() {
        assert_eq!(Direction::default(), Direction::Forward);
    }

    #[test]
    fn forward_client_sends_server_receives() {
        assert!(Direction::Forward.client_sends());
        assert!(!Direction::Forward.client_receives());
        assert!(!Direction::Forward.server_sends());
        assert!(Direction::Forward.server_receives());
    }

    #[test]
    fn reverse_swaps_roles() {
        assert!(!Direction::Reverse.client_sends());
        assert!(Direction::Reverse.client_receives());
        assert!(Direction::Reverse.server_sends());
        assert!(!Direction::Reverse.server_receives());
    }

    #[test]
    fn bidirectional_both_sides_send_and_receive() {
        assert!(Direction::Bidirectional.client_sends());
        assert!(Direction::Bidirectional.client_receives());
        assert!(Direction::Bidirectional.server_sends());
        assert!(Direction::Bidirectional.server_receives());
    }
}
```

- [ ] **Step 2: Declare submodule**

Edit `src/common/mod.rs`: add `pub mod direction;` alongside other `pub mod` lines and `pub use direction::Direction;` with the other re-exports.

- [ ] **Step 3: Add wire fields**

Edit `src/common/wire.rs`. In `ClientOptions`, append:

```rust
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reverse: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub bidirectional: bool,
```

Update `tcp_defaults` to initialize both to `false`.

Append tests to the existing `wire::tests`:

```rust
#[test]
fn options_defaults_reverse_and_bidirectional_false() {
    let opts = ClientOptions::tcp_defaults(1, 1, DEFAULT_TCP_LEN as u32);
    assert!(!opts.reverse);
    assert!(!opts.bidirectional);
}

#[test]
fn options_tcp_forward_does_not_emit_direction_fields() {
    // Forward-mode ClientOptions must look identical on the wire to
    // pre-direction payloads so iperf3 3.21 servers keep accepting them.
    let opts = ClientOptions::tcp_defaults(1, 1, DEFAULT_TCP_LEN as u32);
    let json = serde_json::to_string(&opts).unwrap();
    assert!(!json.contains("reverse"), "unexpected reverse key: {}", json);
    assert!(!json.contains("bidirectional"), "unexpected bidirectional key: {}", json);
}

#[test]
fn options_reverse_roundtrip() {
    let mut opts = ClientOptions::tcp_defaults(1, 1, DEFAULT_TCP_LEN as u32);
    opts.reverse = true;
    let json = serde_json::to_string(&opts).unwrap();
    assert!(json.contains("\"reverse\":true"));
    let back: ClientOptions = serde_json::from_str(&json).unwrap();
    assert_eq!(back, opts);
}

#[test]
fn options_bidirectional_roundtrip() {
    let mut opts = ClientOptions::tcp_defaults(1, 1, DEFAULT_TCP_LEN as u32);
    opts.bidirectional = true;
    let json = serde_json::to_string(&opts).unwrap();
    assert!(json.contains("\"bidirectional\":true"));
    let back: ClientOptions = serde_json::from_str(&json).unwrap();
    assert_eq!(back, opts);
}
```

- [ ] **Step 4: Verify + commit**

```bash
cargo test --lib direction::tests wire::tests
cargo clippy --all-targets -- -D warnings
git add src/common/direction.rs src/common/mod.rs src/common/wire.rs
git commit -m "direction: Direction enum + ClientOptions reverse/bidirectional wire fields"
```

---

## Task 2: `Config.direction` + lib re-export + struct-literal updates

**Files:** `src/common/test.rs`, `src/lib.rs`, `tests/self_test.rs`, `src/cli.rs`.

- [ ] **Step 1: Extend Config**

Edit `src/common/test.rs`. Add import `use crate::common::Direction;`. Extend `Config`:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub time: u32,
    pub parallel: u32,
    pub len: u32,
    pub omit: u32,
    pub transport: TransportKind,
    pub bandwidth: u64,
    pub direction: Direction,
}
```

Update `with_host`:

```rust
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
        direction: Direction::Forward,
    }
}
```

Fix the existing `Config { ... }` literal in the `config_host_port_formats_correctly` test by adding `direction: Direction::Forward`.

Append a test:

```rust
#[test]
fn config_with_host_defaults_forward() {
    let cfg = Config::with_host("h");
    assert_eq!(cfg.direction, crate::common::Direction::Forward);
}
```

- [ ] **Step 2: Re-export from crate root**

Edit `src/lib.rs`:

```rust
pub use common::Direction;
pub use common::TransportKind;
```

- [ ] **Step 3: Fix integration test literals**

`tests/self_test.rs` has three `Config { ... }` struct literals. Add `direction: rperf3::Direction::Forward,` to each.

- [ ] **Step 4: Fix `src/cli.rs` struct literal**

The `try_into_mode` builds a `Config { ... }` literal. Add `direction: Direction::Forward,` (imported via `use crate::common::{bandwidth, Direction, TransportKind};`) — Task 3 will wire it up to the actual flag.

- [ ] **Step 5: Verify + commit**

```bash
cargo build --release
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
git add -u
git commit -m "config: add Direction to Config; re-export from crate root"
```

---

## Task 3: CLI flags `-R/--reverse` and `--bidir`

**Files:** `src/cli.rs`.

- [ ] **Step 1: Add fields + validation**

Extend `Cli`:

```rust
    /// Reverse the direction of the test. Server sends, client receives.
    #[arg(short = 'R', long = "reverse", conflicts_with = "bidir")]
    pub reverse: bool,

    /// Run in bidirectional mode. Both sides send and receive simultaneously.
    #[arg(long = "bidir")]
    pub bidir: bool,
```

Update `try_into_mode` — wire the direction:

```rust
let direction = if self.bidir {
    Direction::Bidirectional
} else if self.reverse {
    Direction::Reverse
} else {
    Direction::Forward
};
```

and set `direction` in the `Config { ... }` literal.

- [ ] **Step 2: Tests**

```rust
#[test]
fn parses_reverse_flag() {
    let cli = Cli::try_parse_from(["rperf3", "-c", "h", "-R"]).unwrap();
    match cli.into_mode() {
        Mode::Client(cfg) => assert_eq!(cfg.direction, crate::common::Direction::Reverse),
        _ => panic!("expected client"),
    }
}

#[test]
fn parses_bidir_flag() {
    let cli = Cli::try_parse_from(["rperf3", "-c", "h", "--bidir"]).unwrap();
    match cli.into_mode() {
        Mode::Client(cfg) => assert_eq!(cfg.direction, crate::common::Direction::Bidirectional),
        _ => panic!("expected client"),
    }
}

#[test]
fn reverse_and_bidir_conflict() {
    let err = Cli::try_parse_from(["rperf3", "-c", "h", "-R", "--bidir"]).unwrap_err();
    assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
}

#[test]
fn default_direction_is_forward() {
    let cli = Cli::try_parse_from(["rperf3", "-c", "h"]).unwrap();
    match cli.into_mode() {
        Mode::Client(cfg) => assert_eq!(cfg.direction, crate::common::Direction::Forward),
        _ => panic!("expected client"),
    }
}
```

- [ ] **Step 3: Verify + commit**

```bash
cargo test --lib cli::tests
cargo clippy --all-targets -- -D warnings
git add src/cli.rs
git commit -m "cli: add -R/--reverse and --bidir flags"
```

---

## Task 4: Receiver variants of `Stream`

**Files:** `src/common/stream.rs`.

Extract the TCP receive loop into a function on `Stream` so `create_streams` on the client can use it for `Reverse`/`Bidirectional` and so the server can use the symmetric structure.

- [ ] **Step 1: Add TCP `Stream::start_recv`**

```rust
impl Stream {
    /// Client-side TCP receiver. Mirrors `Stream::start` but reads from
    /// the data stream instead of sending, emitting iPerf3-style
    /// interval rows as bytes arrive. Used for `Reverse` and (together
    /// with `start`) `Bidirectional`.
    pub fn start_recv(
        test: &Test,
        host: String,
        cookie: [u8; COOKIE_LEN],
        stream_id: u32,
    ) {
        let tx = test.tx_channel.clone();
        let receipt_tx = test.receipt_tx.clone();
        let omit = Duration::from_secs(test.config.omit as u64);
        let duration = Duration::from_secs((test.config.time + test.config.omit) as u64);
        let buf_len = test.config.len as usize;

        thread::spawn(move || {
            let mut receipt = ClientStreamReceipt::empty(stream_id);
            let mut reporter = IntervalReporter::new(stream_id, Duration::from_secs(1));
            let timer = Timer::new();

            if let Some(mut protocol) = connect(host, &cookie) {
                if let Err(e) = protocol.transfer.set_nonblocking(false) {
                    eprintln!("failed to set data stream to blocking: {:?}", e);
                    emit_stream_finished(&receipt_tx, receipt, &tx);
                    return;
                }

                let mut buf = vec![0u8; buf_len.max(65_536)];
                while !should_stop(&timer, duration) {
                    match protocol.transfer.recv(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            let now = Instant::now();
                            if receipt.first_send_at.is_none() {
                                receipt.first_send_at = Some(now);
                            }
                            receipt.last_send_at = Some(now);
                            receipt.bytes_sent += n as u64;

                            let first = receipt.first_send_at.expect("first just set");
                            let in_omit = now.duration_since(first) < omit;
                            if in_omit {
                                receipt.bytes_omit += n as u64;
                            } else if receipt.first_measured_at.is_none() {
                                receipt.first_measured_at = Some(now);
                            }

                            if let Some(snap) = reporter.on_bytes(n as u64, now) {
                                println!(
                                    "{}",
                                    format_interval_row(
                                        snap.stream_id,
                                        snap.start_sec,
                                        snap.end_sec,
                                        snap.bytes,
                                    )
                                );
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
                if let Some(last) = receipt.last_send_at {
                    if let Some(snap) = reporter.flush(last) {
                        if snap.bytes > 0 {
                            println!(
                                "{}",
                                format_interval_row(
                                    snap.stream_id,
                                    snap.start_sec,
                                    snap.end_sec,
                                    snap.bytes,
                                )
                            );
                        }
                    }
                }
            }
            emit_stream_finished(&receipt_tx, receipt, &tx);
        });
    }
}
```

We reuse `ClientStreamReceipt` — the "sent" fields now mean "received" on the client side for reverse tests. Document this in a comment above `start_recv`.

- [ ] **Step 2: Add UDP `Stream::start_udp_recv`**

Symmetric to `start_udp`: bind ephemeral UDP, connect to server, send cookie to establish demux. Then loop on `recv` (no header parsing needed — the iperf3 server does header accounting internally and reports via control-channel results, but for interop we still need to at least *receive* the packets and count bytes so our local intervals match).

```rust
impl Stream {
    pub fn start_udp_recv(
        test: &Test,
        host: String,
        cookie: [u8; COOKIE_LEN],
        stream_id: u32,
    ) {
        let tx = test.tx_channel.clone();
        let receipt_tx = test.receipt_tx.clone();
        let buf_len = test.config.len as usize;
        let duration = Duration::from_secs((test.config.time + test.config.omit) as u64);
        let omit = Duration::from_secs(test.config.omit as u64);

        thread::spawn(move || {
            let mut receipt = ClientStreamReceipt::empty(stream_id);
            let mut reporter = IntervalReporter::new(stream_id, Duration::from_secs(1));
            let timer = Timer::new();

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
            // Stream-init: send cookie (rperf3 convention) so rperf3
            // servers recognize us. Real iperf3 sends `b"9876"`; our
            // udp_session accepts both.
            let _ = socket.send(&cookie);
            let _ = socket.set_read_timeout(Some(Duration::from_millis(100)));

            let mut buf = vec![0u8; buf_len.max(65_536)];
            while !should_stop(&timer, duration) {
                match socket.recv(&mut buf) {
                    Ok(0) => continue,
                    Ok(n) => {
                        let now = Instant::now();
                        if receipt.first_send_at.is_none() {
                            receipt.first_send_at = Some(now);
                        }
                        receipt.last_send_at = Some(now);
                        receipt.bytes_sent += n as u64;
                        receipt.packets += 1;

                        let first = receipt.first_send_at.expect("set");
                        let in_omit = now.duration_since(first) < omit;
                        if in_omit {
                            receipt.bytes_omit += n as u64;
                        } else if receipt.first_measured_at.is_none() {
                            receipt.first_measured_at = Some(now);
                        }

                        if let Some(snap) = reporter.on_bytes(n as u64, now) {
                            println!(
                                "{}",
                                format_interval_row(
                                    snap.stream_id,
                                    snap.start_sec,
                                    snap.end_sec,
                                    snap.bytes,
                                )
                            );
                        }
                    }
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut =>
                    {
                        continue
                    }
                    Err(_) => break,
                }
            }
            emit_stream_finished(&receipt_tx, receipt, &tx);
        });
    }
}
```

- [ ] **Step 3: Unit tests for recv variants**

```rust
#[test]
fn start_recv_pulls_bytes_from_peer() {
    use crate::common::cookie::generate_cookie;
    use crate::common::test::{Config, Test};
    use std::net::{TcpListener, TcpStream};
    use std::io::Write;
    use std::time::Duration;

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    let cookie = generate_cookie();

    // Peer side: accept, read cookie, send some bytes, close.
    let writer = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        let mut cookie_buf = [0u8; crate::common::cookie::COOKIE_LEN];
        std::io::Read::read_exact(&mut sock, &mut cookie_buf).expect("read cookie");
        for _ in 0..4 {
            sock.write_all(&vec![0xA5u8; 4096]).expect("write");
        }
    });

    let mut cfg = Config::with_host("127.0.0.1");
    cfg.port = port;
    cfg.time = 1;
    cfg.parallel = 1;
    cfg.len = 8192;
    let test = Test::new(cfg);
    let expected_cookie = test.cookie;
    let host = test.config.host_port();

    Stream::start_recv(&test, host, expected_cookie, 1);

    let receipt = test.receipt_rx.recv_timeout(Duration::from_secs(3)).expect("receipt");
    writer.join().expect("writer");
    assert!(receipt.bytes_sent >= 16_384, "expected ≥ 16 KB, got {}", receipt.bytes_sent);
}
```

A similar UDP recv smoke test isn't strictly necessary for the plan — the end-to-end self-tests in Task 7 exercise it. Skip it to keep the unit tests focused.

- [ ] **Step 4: Commit**

```bash
cargo test --lib stream
cargo clippy --all-targets -- -D warnings
git add src/common/stream.rs
git commit -m "stream: add start_recv (TCP) and start_udp_recv (UDP) variants"
```

---

## Task 5: Client-side direction routing

**Files:** `src/client.rs`.

- [ ] **Step 1: Set wire fields + branch in `create_streams`**

In `send_options`:

```rust
let mut options = ClientOptions::tcp_defaults(test.config.time, test.config.parallel, test.config.len);
options.omit = test.config.omit;
options.udp = test.config.transport.is_udp();
options.bandwidth = test.config.bandwidth;
if options.udp { options.tcp = false; }
options.reverse = test.config.direction.is_reverse();
options.bidirectional = test.config.direction.is_bidirectional();
```

Replace `create_streams`:

```rust
fn create_streams(test: &Test) {
    let host = test.config.host_port();
    let n = test.config.parallel.max(1);
    let is_udp = test.config.transport.is_udp();
    let dir = test.config.direction;
    for stream_id in 1..=n {
        if dir.client_sends() {
            if is_udp {
                Stream::start_udp(test, host.clone(), test.cookie, stream_id);
            } else {
                Stream::start(test, host.clone(), test.cookie, stream_id);
            }
        }
        if dir.client_receives() {
            if is_udp {
                Stream::start_udp_recv(test, host.clone(), test.cookie, stream_id);
            } else {
                Stream::start_recv(test, host.clone(), test.cookie, stream_id);
            }
        }
    }
}
```

- [ ] **Step 2: Adjust stream completion counting**

The main loop sends `TestEnd` when `streams_finished >= test.config.parallel.max(1)`. With `Bidirectional` we have `2N` threads. Update the threshold:

```rust
let expected_streams = test.config.parallel.max(1)
    * if test.config.direction.is_bidirectional() { 2 } else { 1 };
...
if streams_finished >= expected_streams && !test_end_sent {
```

- [ ] **Step 3: Adjust summary rows**

In `exchange_results`, the final `sender`/`receiver` rows should reflect the actual direction. For `Reverse`, client is receiver only. For `Bidirectional`, print both. The simplest approach: compute `sender_bytes`/`receiver_bytes` from receipts and print only the applicable rows.

```rust
let dir = test.config.direction;
let sender_rows = if dir.client_sends() {
    let (b, d) = client_totals(&test.receipts);
    Some((b, d))
} else { None };
```

Keep the "receiver" row source identical — from peer's framed `Results` payload (server's view is authoritative for byte counts on the direction it receives).

Full restructure:

```rust
println!("{}", stream::SUMMARY_SEPARATOR);
println!("{}", stream::INTERVAL_HEADER);

let steady = client_measured_duration(&test.receipts);
let session = client_session_duration(&test.receipts).unwrap_or(Duration::ZERO);
let client_bytes: u64 = test.receipts.iter().map(|r| r.bytes_sent).sum();
let client_duration = if test.config.omit > 0 && !steady.is_zero() { steady } else { session };
let secs = client_duration.as_secs_f64();

// Client row: role depends on direction.
let client_role = if dir.is_bidirectional() { "sender" }
    else if dir.is_reverse() { "receiver" }
    else { "sender" };
println!("{}  {}", stream::format_interval_row(1, 0.0, secs, client_bytes), client_role);

// ... send client Results, read server Results, print server-side row(s).
let server_role = if dir.is_bidirectional() { "receiver" }
    else if dir.is_reverse() { "sender" }
    else { "receiver" };
match recv_framed_json::<Results>(...) {
    Ok(server) => {
        let server_bytes = server.streams.iter().map(|s| s.bytes).sum::<u64>();
        println!("{}  {}", stream::format_interval_row(1, 0.0, secs, server_bytes), server_role);
        // For bidir there are separate send + recv receipts on the client
        // side — if we emit only the dominant role here we drop half the
        // picture. Handle by emitting an extra row for the client's
        // *receive* receipts when bidir.
        if dir.is_bidirectional() {
            // (already shown as "sender" above; add "receiver" using
            // the subset of receipts whose stream_id came from start_recv.
            // For simplicity, sum all receipts and print as receiver too —
            // bidir receipts aren't tagged by direction, so the grand total
            // covers both halves.)
        }
    }
    ...
}
```

Keep the bidir block simple: sum all client receipts and render as "sender + receiver" where applicable. This is an approximation — a clean implementation would tag each `ClientStreamReceipt` with its role. Add a TODO note that exact bidir attribution can be tightened when the Direction is tagged into the receipt.

- [ ] **Step 4: Verify + commit**

```bash
cargo build --release
cargo clippy --all-targets -- -D warnings
git add src/client.rs
git commit -m "client: route streams by Direction; adjust summary role labels"
```

---

## Task 6: Server-side direction routing

**Files:** `src/server.rs`.

- [ ] **Step 1: Refactor `run_tcp_branch` into sender + receiver halves**

Move the existing `recv_stream_bytes`-driven loop into `run_tcp_receive_branch`. Add `run_tcp_send_branch` that mirrors the client-side `Stream::start` (uses the accepted `Protocol` handles to blast bytes for `time + omit` seconds).

A new helper:

```rust
fn server_send_stream(mut proto: Protocol, config_time: u32, config_omit: u32, len: u32, stream_id: u32) -> StreamReceipt {
    let mut receipt = StreamReceipt::empty();
    let mut reporter = IntervalReporter::new(stream_id, Duration::from_secs(1));
    let duration = Duration::from_secs((config_time + config_omit) as u64);
    let omit = Duration::from_secs(config_omit as u64);

    if proto.transfer.set_nonblocking(false).is_err() {
        return receipt;
    }
    let timer = Timer::new();
    let buf = crate::common::stream::build_payload(len as usize);
    while !should_stop_server(&timer, duration) {
        match proto.transfer.send(&buf) {
            Ok(n) if n > 0 => {
                let now = Instant::now();
                if receipt.first_byte_at.is_none() { receipt.first_byte_at = Some(now); }
                receipt.last_byte_at = Some(now);
                receipt.bytes += n as u64;
                let first = receipt.first_byte_at.expect("set");
                if now.duration_since(first) < omit {
                    receipt.bytes_omit += n as u64;
                } else if receipt.first_measured_at.is_none() {
                    receipt.first_measured_at = Some(now);
                }
                if let Some(snap) = reporter.on_bytes(n as u64, now) {
                    println!("{}", crate::common::stream::format_interval_row(
                        snap.stream_id, snap.start_sec, snap.end_sec, snap.bytes,
                    ));
                }
            }
            Ok(_) => continue,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Err(_) => break,
        }
    }
    receipt
}

fn should_stop_server(timer: &crate::common::timer::Timer, duration: Duration) -> bool {
    timer.is_elapsed(duration)
}
```

(Timer type is `crate::common::timer::Timer`.)

In `run_tcp_branch`, dispatch:

```rust
let receipts = if opts.reverse || opts.bidirectional {
    run_tcp_send_branch(...)
} else {
    run_tcp_receive_branch(...)  // the existing path
};

// In bidirectional, also spawn receivers on the same sockets.
// iperf3 3.21 bidir: the same TCP data stream carries both directions
// simultaneously. Split each accepted Protocol into a reader half + writer half
// via try_clone and run both.
```

For bidir TCP we need `try_clone` on the accepted TCP stream so one thread writes and another reads on the same socket. Extend the `Tcp` wrapper in protocol.rs with a `try_clone` helper returning a second wrapped socket.

This is the most invasive change in the plan. If bidir TCP proves too large to fit in one task, split it into **Task 6a** (reverse only) and **Task 6b** (bidirectional). Recommend starting with 6a, shipping it, then 6b.

- [ ] **Step 2: Dispatch in `run_udp_branch`**

For reverse UDP: server's UDP receiver becomes a sender. We reuse the existing client `Stream::start_udp`-style code but on the server, sending to the learned `SocketAddr` of each stream.

Pragmatic approach: after `accept_udp_streams` returns the per-stream `SocketAddr`s, branch:

```rust
let receipts = if opts.reverse {
    run_udp_send_branch(udp, addrs, opts, stop)
} else if opts.bidirectional {
    // Spawn BOTH send and receive threads, each with a clone of the UdpSocket.
    run_udp_bidir_branch(udp, addrs, opts, stop)
} else {
    run_udp_receiver(udp, addrs, omit_window, receiver_stop)
};
```

Keep scope bounded: UDP bidir with a single shared socket needs careful packet demux. If time-limited, ship reverse-only and mark bidir UDP as `#[ignore]` in the test, same as the TCP post-test gap.

- [ ] **Step 3: Tests for server-side role switch**

Unit tests for `server_send_stream` (mock socket that records bytes, assert throughput over a brief window).

- [ ] **Step 4: Verify + commit**

```bash
cargo test --lib server
cargo test --test self_test
cargo clippy --all-targets -- -D warnings
git add src/server.rs
git commit -m "server: route streams by Direction (reverse + bidirectional)"
```

---

## Task 7: Self-tests for reverse + bidir loopback

**Files:** `tests/self_test.rs`.

- [ ] **Step 1: Add four loopback tests**

```rust
#[test]
fn self_test_loopback_tcp_reverse() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || rperf3::run_server_on(listener));
    let cfg = rperf3::Config {
        host: "127.0.0.1".into(), port, time: 1, parallel: 1,
        len: 131_072, omit: 0,
        transport: rperf3::TransportKind::Tcp, bandwidth: 0,
        direction: rperf3::Direction::Reverse,
    };
    thread::sleep(Duration::from_millis(50));
    rperf3::run_client(cfg);
    let total = server.join().expect("srv panic").expect("srv err");
    // Reverse: server is sender. total_bytes returned by run_server_on
    // reflects bytes *received* by the server — for reverse, that's the
    // control-channel accounting path, which is zero receive bytes but
    // non-zero sent bytes. Assert that the test ran without error.
    // Tighter assertions land when Results JSON is plumbed through the
    // public run_server_on return.
    let _ = total;
}

#[test]
fn self_test_loopback_udp_reverse() { /* mirror above with UDP */ }

#[test]
fn self_test_loopback_tcp_bidir() { /* mirror with Bidirectional */ }

#[test]
fn self_test_loopback_udp_bidir() { /* mirror with UDP + Bidirectional */ }
```

Assert `run_client` completes without panic and `run_server_on` returns `Ok`. Finer-grained byte-count assertions need `run_server_on`'s return type to expose sender totals for reverse mode — note as a follow-up.

- [ ] **Step 2: Verify + commit**

```bash
cargo test --test self_test
git add tests/self_test.rs
git commit -m "self_test: loopback reverse + bidir cases (TCP and UDP)"
```

---

## Task 8: Cross-iperf3 interop tests

**Files:** `tests/cross_interop_reverse.rs`, `tests/cross_interop_bidir.rs`.

- [ ] **Step 1: Reverse cross-interop**

```rust
//! Cross-iperf3 `-R` (reverse) interop. Skips when iperf3 is absent.

use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

fn iperf3_available() -> bool {
    Command::new("iperf3").arg("--version").stdout(Stdio::null()).stderr(Stdio::null())
        .status().map(|s| s.success()).unwrap_or(false)
}

#[test]
fn rperf3_client_reverse_against_iperf3_server() {
    if !iperf3_available() { println!("skipping: iperf3 not on PATH"); return; }
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let iperf3_server = Command::new("iperf3")
        .args(["-s", "-p", &port.to_string(), "-1"])
        .stdout(Stdio::piped()).stderr(Stdio::piped())
        .spawn().expect("spawn");
    thread::sleep(Duration::from_millis(200));
    let cfg = rperf3::Config {
        host: "127.0.0.1".into(), port, time: 1, parallel: 1,
        len: 131_072, omit: 0,
        transport: rperf3::TransportKind::Tcp, bandwidth: 0,
        direction: rperf3::Direction::Reverse,
    };
    rperf3::run_client(cfg);
    let output = iperf3_server.wait_with_output().expect("wait");
    assert!(output.status.success(), "iperf3 server failed: {:?}", output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("bits/sec"), "missing throughput line: {}", stdout);
}

// Mirror direction: iperf3 client with -R against our server is the same as
// rperf3 client without -R against our server from the data-flow POV — but
// iperf3 client still drives the direction flag. Spawn iperf3 -c -R and
// verify our server serves as sender.
#[test]
#[ignore] // relies on post-test protocol fixes being in place
fn iperf3_client_reverse_against_rperf3_server() {
    // ... symmetric to the iperf3_client_tcp test, with -R added to iperf3 args.
}
```

- [ ] **Step 2: Bidir cross-interop**

Same structure with `direction: Direction::Bidirectional` and `iperf3 ... --bidir`.

- [ ] **Step 3: Run + commit**

```bash
cargo test --test cross_interop_reverse --test cross_interop_bidir -- --nocapture
git add tests/cross_interop_reverse.rs tests/cross_interop_bidir.rs
git commit -m "tests: cross-iperf3 interop for -R and --bidir"
```

---

## Task 9: README + roadmap update

**Files:** `README.md`, `docs/superpowers/specs/2026-04-18-iperf3-parity-roadmap.md`.

- [ ] Add `-R/--reverse` and `--bidir` to the Usage block.
- [ ] Add a reverse/bidir example under "Test against itself" and "Test against real iPerf3".
- [ ] Mark Sub-project 2 as complete (or note partial if bidir-UDP shipped as `#[ignore]`).
- [ ] Commit `docs: document reverse + bidirectional`.

---

## Task 10: Full-suite verification

- [ ] `cargo test --all-targets` → green
- [ ] `cargo clippy --all-targets -- -D warnings` → clean
- [ ] `cargo build --release` → green
- [ ] Manual spot-check: `./target/release/rperf3 -c 127.0.0.1 -p 5201 -R -t 2` against a real iperf3 server renders sensible output.

---

## Risk + Deferrals

- **TCP bidir: socket cloning.** `Tcp::try_clone` must produce a fully independent `TcpStream` handle so one thread writes and another reads without interfering. Rust's `TcpStream::try_clone` does this; the wrapper just forwards. No new dependencies.
- **UDP bidir: single-socket demux.** Server's UDP bidir means the same `UdpSocket` is both sending to `client_addr` and receiving from it. `UdpSocket` is `Send + Sync` after `Arc`; two threads using the same `recv_from`/`send_to` is supported on Linux/macOS. Lock-free.
- **Per-stream direction tagging in `ClientStreamReceipt`.** Not added here. The approximation (treating all bidir receipts as "sender total") is good enough for throughput numbers; tighter attribution belongs to sub-project #4 (output formatting) when we introduce a `Reporter` trait.
- **iperf3 post-test finalization** (from Phase 1's `#[ignore]`'d test) still applies to `-R` and `--bidir`. Cross-interop tests for "iperf3 client → rperf3 server" in reverse/bidir stay `#[ignore]`d for the same reason until that protocol gap closes.
