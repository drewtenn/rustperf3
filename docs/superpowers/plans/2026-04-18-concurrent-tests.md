# Concurrent Tests Implementation Plan

> **For agentic workers:** Use superpowers:subagent-driven-development. Checkboxes track progress.

**Goal:** Allow the server to serve multiple clients. Default behavior matches iperf3: loop forever, one test at a time, rejecting new connections during an active test with "the server is busy". New `-1` flag preserves the current one-shot behavior. `--max-concurrent N` is an rperf3 extension that allows up to N simultaneous tests via cookie-multiplexing.

**Architecture:** Split the server into an accept loop and a session registry. Every inbound TCP connection reads its 37-byte cookie first; unknown cookies start a new `Session` (registered by cookie in an `Arc<Mutex<HashMap<Cookie, SessionHandle>>>`), known cookies are handed to the matching session as data streams via a `mpsc` channel. Sessions run serve_session on their own thread. When a session finishes, it deregisters from the map.

**Tech Stack:** `std::thread`, `std::sync::{Arc, Mutex, mpsc}`. No new crates.

---

## Tasks

### Task 1: Add `-1` and `--max-concurrent N` CLI flags

Files: `src/cli.rs`.

- [ ] Add to `Cli`:
  ```rust
  #[arg(short = '1', long = "one-off")]
  pub one_off: bool,

  #[arg(long = "max-concurrent", default_value_t = 1)]
  pub max_concurrent: u32,
  ```

- [ ] Extend `Config` (in `src/common/test.rs`) and `try_into_mode` in cli.rs to plumb these. Add fields:
  ```rust
  pub one_off: bool,
  pub max_concurrent: u32,
  ```
  Default `one_off: false, max_concurrent: 1` in `with_host`.

- [ ] Fix existing `Config { ... }` literals: `tests/self_test.rs`, `tests/cross_interop_*.rs` (all 5 files), `src/cli.rs` test literals, `src/common/test.rs` test literal. Add `one_off: false, max_concurrent: 1,`.

- [ ] Tests in cli.rs:
  ```rust
  #[test] fn one_off_default_false() { ... }
  #[test] fn parses_one_off_short() { try_parse ["rperf3", "-s", "-1"] → cli.one_off == true }
  #[test] fn parses_max_concurrent() { try_parse ["rperf3", "-s", "--max-concurrent", "4"] → 4 }
  ```

- [ ] Commit: `cli: add -1/--one-off and --max-concurrent flags`

---

### Task 2: Server loops until signal / -1

Files: `src/server.rs`, `src/main.rs`.

**Intent:** With no `-1`, `run_server` loops forever serving clients sequentially (same as iperf3 default). With `-1`, exits after the first test.

- [ ] In `serve_one`, rename to `serve_one_session` (it's still one session's work).

- [ ] Add a `serve_loop` that wraps the accept loop:
  ```rust
  pub fn run_server_on_with_options(
      listener: TcpListener,
      handshake_timeout: Option<Duration>,
      one_off: bool,
      max_concurrent: u32,
  ) -> io::Result<()> {
      if max_concurrent <= 1 {
          loop {
              match serve_one_session(&listener, handshake_timeout) {
                  Ok(_) => {}
                  Err(e) => eprintln!("session ended with error: {:?}", e),
              }
              if one_off { return Ok(()); }
          }
      } else {
          run_concurrent_accept_loop(listener, handshake_timeout, max_concurrent)
      }
  }
  ```

- [ ] Update `run_server` to pass `config.one_off` / `config.max_concurrent` through.

- [ ] Keep `run_server_on(listener) -> io::Result<u64>` as-is for backward compat with integration tests; have it call `serve_one_session` (one-shot) — tests need the single return value.

- [ ] Commit: `server: loop forever by default; -1 for one-shot`

---

### Task 3: Concurrent accept loop with cookie demux

Files: `src/common/session.rs` (new), `src/server.rs`.

**Design:**

```
accept loop {
    (sock, addr) = listener.accept()
    cookie = read_cookie(sock)
    match registry.lookup(cookie) {
        None => {
            if registry.len() >= max_concurrent { send busy_error; continue; }
            session = Session::new(cookie, sock)
            registry.insert(cookie, session.handle)
            spawn(session.run)
        }
        Some(handle) => handle.push_data_stream(sock)
    }
}
```

`Session::run` owns control-channel state machine; reads data-stream sockets from a `mpsc::Receiver<Protocol>` fed by the accept loop.

- [ ] New file `src/common/session.rs`:
  - `pub struct Session { cookie: Cookie, control: Protocol, data_rx: Receiver<Protocol> }`
  - `pub struct SessionHandle { data_tx: Sender<Protocol> }`
  - `impl Session { fn new(...) -> (Self, SessionHandle) }` — pairs session with its handle; one-shot creation.
  - `impl Session { fn run(self) -> io::Result<u64> }` — runs serve_session_on_preconnected.

- [ ] Refactor `serve_one_session` so the body after accept/cookie-read is reusable as `serve_session_on_preconnected(control, cookie, handshake_timeout, data_rx)`. The `accept_streams` helper changes to pull from `data_rx` instead of calling `listener.accept()` directly.

  Because `accept_streams` in the existing code takes `&TcpListener`, add a new helper `accept_streams_from_channel(data_rx: &Receiver<Protocol>, n: u32, timeout)` that pulls the N streams from the channel with a timeout.

- [ ] Implement `run_concurrent_accept_loop`:
  ```rust
  fn run_concurrent_accept_loop(
      listener: TcpListener,
      handshake_timeout: Option<Duration>,
      max_concurrent: u32,
  ) -> io::Result<()> {
      let registry: Arc<Mutex<HashMap<[u8; COOKIE_LEN], SessionHandle>>>
          = Arc::new(Mutex::new(HashMap::new()));
      loop {
          let (sock, _addr) = listener.accept()?;
          let mut tcp = tcp_protocol(sock);
          tcp.transfer.set_read_timeout(handshake_timeout)?;
          let cookie = match recv_cookie(tcp.transfer.as_mut()) {
              Ok(c) => c,
              Err(e) => { eprintln!("cookie read failed: {:?}", e); continue; }
          };
          let mut reg = registry.lock().unwrap();
          if let Some(handle) = reg.get(&cookie) {
              // Data-stream connection for existing session.
              if handle.data_tx.send(tcp).is_err() {
                  eprintln!("session channel closed; dropping stream");
              }
          } else if reg.len() >= max_concurrent as usize {
              drop(reg);
              // Send "server is busy" byte and close.
              let _ = send_control_byte(tcp.transfer.as_mut(), Message::AccessDenied);
          } else {
              let registry_for_cleanup = registry.clone();
              let (session, handle) = Session::new(cookie, tcp);
              reg.insert(cookie, handle);
              drop(reg);
              std::thread::spawn(move || {
                  let _ = session.run(handshake_timeout);
                  let mut reg = registry_for_cleanup.lock().unwrap();
                  reg.remove(&cookie);
              });
          }
      }
  }
  ```

- [ ] Commit: `server: concurrent accept loop with cookie demux`

---

### Task 4: Self-test for concurrent sessions

Files: `tests/self_test.rs`.

- [ ] New test: spawn an rperf3 server with `max_concurrent=4`, fire 3 concurrent clients at it, assert all three complete with non-zero throughput. Use `std::thread` to parallelize clients.

- [ ] Commit: `self_test: 3 concurrent clients against max-concurrent=4 server`

---

### Task 5: README + roadmap update

- [ ] Add `-1` and `--max-concurrent` to README Usage block.
- [ ] Add "What works today" bullet mentioning server loops + concurrent tests.
- [ ] Update roadmap spec: mark Sub-project 3 as ✅ shipped.

- [ ] Commit: `docs: document concurrent tests`

---

### Task 6: Final verify + merge

- [ ] `cargo test --all-targets` green.
- [ ] `cargo clippy --all-targets -- -D warnings` clean.
- [ ] `cargo build --release`.
