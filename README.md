# rPerf3

A small iPerf3-compatible network throughput tool in Rust.

## Why this exists

I started this project because I ran into an implementation problem in
the Windows build of iPerf3 and wanted a drop-in replacement I could
understand, debug, and extend. Learning Rust along the way was the
bonus.

The goal is protocol-level compatibility with iPerf3 (same wire format,
same control messages, same cookie-multiplexed streams) — not feature
parity. If `rPerf3` client can talk to an iPerf3 server, and an iPerf3
client can talk to `rPerf3` server, the MVP is working.

## What works today

- Client mode (`-c <host>`): control-channel handshake, configurable
  duration / parallel streams / write size, TCP data streams, results
  exchange.
- Server mode (`-s`): accepts one test at a time on a configured port,
  handles the full iPerf3 state machine end to end (ParamExchange →
  CreateStreams → TestStart/Running → TestEnd → ExchangeResults →
  DisplayResults → IperfDone), prints a summary line with observed
  bytes and Mbits/sec.
- CLI: `-s` and `-c` are mutually exclusive, one is required.
- Self-test: `cargo test --test self_test` spins up an in-process
  server and runs a client against it over loopback.
- **UDP data path (`-u`).** Full iPerf3-compatible UDP: 16-byte
  datagram header, RFC 1889 jitter EWMA, lost-packet and out-of-order
  accounting, sender-side bandwidth pacing (`-b`, default 1 Mbps for
  UDP), and an end-of-test sentinel packet. Works bidirectionally
  against real iPerf3.
- **Reverse + bidirectional tests (`-R`, `--bidir`).** In reverse, the
  server is the sender and the client is the receiver. In bidirectional,
  both sides send and receive on the same data streams concurrently.
  Works on TCP and UDP; interoperates with real iPerf3 in the
  rperf3-client → iperf3-server direction.
- **Server loops by default + concurrent tests** (`-1`, `--max-concurrent N`).
  The server now serves tests until killed (iperf3 default), `-1` restores
  the old one-shot behavior, and `--max-concurrent N > 1` enables up to N
  simultaneous tests multiplexed by cookie (TCP only — UDP concurrent
  sessions are rejected with AccessDenied).

### Accuracy features

Built in because this is meant to be a trustworthy view of bandwidth,
not just "a number":

- **Measured elapsed on both sides.** Every stream records the
  timestamps of its first and last bytes. Throughput is computed
  from (last − first), not from the client-advertised integer
  seconds. Sub-second precision end to end.
- **`-O/--omit N`.** Skip the first N seconds of each stream so TCP
  slow-start doesn't pull the reported steady-state number down.
  Bytes from the omit window are tracked separately and excluded from
  the Mbits/sec line.
- **Per-second interval rows** on both client and server, so you can
  see whether a flat 1 Gbps is actually flat or oscillating:
  ```
  [CLI 1] 0.00-1.00 sec  128 MBytes  1.07 Gbits/sec
  [SRV 1] 0.00-1.00 sec  128 MBytes  1.07 Gbits/sec
  ```
- **TCP retransmits** (Linux, via `getsockopt(TCP_INFO)`). A clean
  throughput number on a lossy link isn't clean — retransmits would
  have been silent. Now they aren't.
- **CPU utilization** (Linux, via `/proc/self/stat`): helps tell
  "the network is slow" from "I'm CPU-bound."
- **Control-channel read timeouts.** A vanished peer no longer hangs
  the server forever; default handshake timeout is 30 seconds and
  the live-test window gets `handshake_timeout + test_duration`.

## Usage

```
Usage: rperf3 [OPTIONS] <--server|--client <HOST>>

Options:
  -s, --server               Run as a server (listen for incoming connections)
  -c, --client <HOST>        Run as a client and connect to this server host
  -p, --port <PORT>          Server port [default: 5201]
  -t, --time <TIME>          Test duration in seconds [default: 10]
  -P, --parallel <PARALLEL>  Number of parallel streams [default: 1]
  -l, --len <LEN>            Bytes per write (TCP buffer length) [default: 131072]
  -u, --udp                  Use UDP instead of TCP for data streams
  -b, --bandwidth <RATE>     Target bandwidth for UDP sender (e.g. 100M, 1G, 0 = unlimited)
  -R, --reverse              Reverse direction — server sends, client receives
      --bidir                Bidirectional — both sides send and receive
  -O, --omit <OMIT>          Seconds to omit at the start of the test [default: 0]
  -1, --one-off              Exit the server after one test (iperf3 -1 equivalent)
      --max-concurrent <N>   Max concurrent sessions [default: 1]
  -h, --help                 Print help
  -V, --version              Print version
```

### Test against itself

```
# Terminal A
cargo run --release -- -s -p 5202

# Terminal B
cargo run --release -- -c 127.0.0.1 -p 5202 -t 3 -P 2
```

# Run the server indefinitely, handle up to 4 simultaneous tests
cargo run --release -- -s -p 5202 --max-concurrent 4

Or run the in-process self-test:

```
cargo test --test self_test -- --nocapture
```

```
# Reverse TCP: server sends, client receives
cargo run --release -- -c 127.0.0.1 -p 5202 -R -t 3
```

UDP at 100 Mbps for 3 seconds:

```
# Terminal A
cargo run --release -- -s -p 5202 -u

# Terminal B
cargo run --release -- -c 127.0.0.1 -p 5202 -u -b 100M -t 3
```

### Test against real iPerf3

rPerf3 client ↔ iperf3 server:

```
iperf3 -s -p 5202
cargo run --release -- -c 127.0.0.1 -p 5202 -t 3
```

rPerf3 client ↔ iperf3 server (UDP):

```
iperf3 -s -p 5202
cargo run --release -- -c 127.0.0.1 -p 5202 -u -b 50M -t 3
```

iperf3 client ↔ rPerf3 server (UDP):

```
cargo run --release -- -s -p 5202
iperf3 -c 127.0.0.1 -p 5202 -u -b 50M -t 3
```

rperf3 client in reverse ↔ iperf3 server:

```
iperf3 -s -p 5202
cargo run --release -- -c 127.0.0.1 -p 5202 -R -t 3
```

## Building

```
cargo build --release
cargo test            # unit + integration tests
cargo clippy --all-targets -- -D warnings
```

## Project layout

```
src/
  main.rs              CLI entry point
  lib.rs               crate::run_client / run_server / run_server_on
  cli.rs               clap-derive argument parsing and Mode enum
  client.rs            client state machine
  server.rs            server state machine
  common/
    mod.rs             Message enum + connect()
    cookie.rs          session cookie generation and wire reading
    cpu.rs             /proc/self/stat CPU sampling (Linux)
    interval.rs        per-second rolling interval reporter
    protocol.rs        Socket trait, TCP/UDP wrappers, test MockSocket
    stream.rs          data-stream worker thread
    test.rs            Test + Config + per-stream receipts
    timer.rs           std::time::Instant wrapper
    wire.rs            framed JSON helpers + ClientOptions/Results
tests/
  self_test.rs         end-to-end loopback tests (data flow, omit, timeout)
```

## Out of scope

- Async/tokio runtime
- TLS / authentication
- Window size (`-w`) negotiation and reporting
