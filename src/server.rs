//! iPerf3-compatible server mode.
//!
//! Mirrors the client state machine from the other side of the wire:
//! accepts a control connection, reads the cookie, negotiates options,
//! opens data streams, receives bytes until the client says TestEnd,
//! then exchanges results. The functions in this module are built
//! small so each piece can be unit-tested against a loopback
//! `TcpListener` without spinning up the full binary.

use std::io;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::common::cookie::{recv_cookie, COOKIE_LEN};
use crate::common::udp_session::{accept_udp_streams, bind_udp, run_udp_receiver};
use crate::common::cpu::{self, CpuUsage};
use crate::common::interval::IntervalReporter;
use crate::common::protocol::{Protocol, Tcp};
use crate::common::test::Config;
use crate::common::wire::{
    recv_control_byte, recv_framed_json, send_control_byte, send_framed_json, ClientOptions,
    Results, StreamResults,
};
use crate::common::Message;

/// What a single reader thread observed. Keeps byte counts split into
/// omit / measured plus the timestamps of the first byte, first
/// post-omit byte, and last byte. This lets the server report steady-
/// state throughput separately from the full test window.
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

    /// Bytes attributed to the measurement window (post-omit).
    pub fn bytes_measured(&self) -> u64 {
        self.bytes.saturating_sub(self.bytes_omit)
    }

    /// Duration between first and last observed byte on this stream,
    /// or zero if no bytes were seen. Includes the omit window.
    pub fn elapsed(&self) -> Duration {
        match (self.first_byte_at, self.last_byte_at) {
            (Some(f), Some(l)) if l >= f => l - f,
            _ => Duration::ZERO,
        }
    }

    /// Wall-clock span from first post-omit byte to last byte.
    pub fn measured_elapsed(&self) -> Duration {
        match (self.first_measured_at, self.last_byte_at) {
            (Some(f), Some(l)) if l >= f => l - f,
            _ => Duration::ZERO,
        }
    }
}

/// Default read timeout applied to the control channel during the
/// handshake phases (before and after the data-transfer window).
/// Generous enough for slow networks, tight enough that a vanished peer
/// unblocks the server within half a minute.
pub const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// Entry point invoked from `main` when `-s` is given. Binds the
/// configured address, handles one test session, and returns.
pub fn run_server(config: Config) {
    let bind_addr = config.host_port();
    let listener = match TcpListener::bind(&bind_addr) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("failed to bind {}: {:?}", bind_addr, e);
            return;
        }
    };
    println!("rperf server listening on {}", bind_addr);

    if let Err(e) = run_server_on(listener) {
        eprintln!("server session ended with error: {:?}", e);
    }
}

/// Drive a full session against a pre-bound `TcpListener`. Returns the
/// total number of bytes received across all data streams. Broken out
/// from `run_server` so integration tests (and callers that want to
/// grab an ephemeral port with `127.0.0.1:0`) can bind first and hand
/// the listener in.
pub fn run_server_on(listener: TcpListener) -> io::Result<u64> {
    run_server_on_timeout(listener, Some(DEFAULT_HANDSHAKE_TIMEOUT))
}

/// Like `run_server_on` but lets callers override the handshake
/// timeout. Passing `None` disables timeouts (the legacy
/// hang-indefinitely-on-silent-peer behavior). Primarily useful for
/// integration tests that want to provoke a fast-failing timeout.
pub fn run_server_on_timeout(
    listener: TcpListener,
    handshake_timeout: Option<Duration>,
) -> io::Result<u64> {
    serve_one(&listener, handshake_timeout)
}

/// Drive a single test session end-to-end. Returns the total bytes
/// received across all data streams so callers (the binary's
/// `run_server`, integration tests) can surface it.
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
    let local = listener.local_addr()?;
    let bind_str = format!("{}:{}", local.ip(), local.port());
    let udp = bind_udp(&bind_str, handshake_timeout)?;

    send_control_byte(control.transfer.as_mut(), Message::CreateStreams)?;
    let addrs = accept_udp_streams(&udp, cookie, opts.parallel)?;
    println!("accepted {} udp stream(s)", addrs.len());

    udp.set_read_timeout(None)?;

    send_test_start_running(control)?;

    let run_timeout = handshake_timeout
        .map(|h| h + Duration::from_secs((opts.time + opts.omit) as u64));
    control.transfer.set_read_timeout(run_timeout)?;

    let stop = Arc::new(AtomicBool::new(false));
    let receiver_stop = stop.clone();
    let omit_window = Duration::from_secs(opts.omit as u64);
    let handle = std::thread::spawn(move || {
        run_udp_receiver(udp, addrs, omit_window, receiver_stop)
    });

    wait_for_test_end(control)?;
    stop.store(true, std::sync::atomic::Ordering::Relaxed);

    let receipts = handle.join().unwrap_or_default();
    Ok(receipts)
}

/// Send DisplayResults and block until the client sends IperfDone. This
/// completes the iPerf3 protocol and lets both sides drop their
/// sockets cleanly.
pub fn finalize_test(control: &mut Protocol) -> io::Result<()> {
    send_control_byte(control.transfer.as_mut(), Message::DisplayResults)?;
    loop {
        match recv_control_byte(control.transfer.as_mut())? {
            Message::IperfDone => return Ok(()),
            other => eprintln!("unexpected control byte while awaiting IperfDone: {:?}", other),
        }
    }
}

/// Produce a human-readable summary line. Isolated so it's trivially
/// testable without touching stdout.
pub fn format_summary(total_bytes: u64, duration: std::time::Duration, streams: usize) -> String {
    let secs = duration.as_secs_f64().max(0.000_001);
    let mbits = (total_bytes as f64 * 8.0) / 1_000_000.0 / secs;
    format!(
        "received {} bytes across {} stream(s) in {:.2}s ({:.2} Mbits/sec)",
        total_bytes, streams, secs, mbits
    )
}

fn print_summary(line: &str) {
    println!("[SUMMARY] {}", line);
}

/// Block on the control channel until the client sends `TestEnd`. Any
/// other control byte is unexpected here and gets logged but does not
/// abort (matches iPerf3's tolerance of extra frames).
pub fn wait_for_test_end(control: &mut Protocol) -> io::Result<()> {
    loop {
        match recv_control_byte(control.transfer.as_mut())? {
            Message::TestEnd => return Ok(()),
            other => eprintln!("unexpected control byte while awaiting TestEnd: {:?}", other),
        }
    }
}

/// Join every reader handle, returning one `StreamReceipt` per stream.
/// Panicked threads contribute an empty receipt.
pub fn join_stream_totals(
    handles: Vec<std::thread::JoinHandle<StreamReceipt>>,
) -> Vec<StreamReceipt> {
    handles
        .into_iter()
        .map(|h| h.join().unwrap_or_else(|_| StreamReceipt::empty()))
        .collect()
}

/// Duration spanned by the earliest `first_byte_at` to the latest
/// `last_byte_at` across every stream. This is the actual wall-clock
/// window data was flowing on the server side — the number to use for
/// throughput calculations instead of the client-advertised duration.
pub fn measured_duration(receipts: &[StreamReceipt]) -> Duration {
    let first = receipts.iter().filter_map(|r| r.first_byte_at).min();
    let last = receipts.iter().filter_map(|r| r.last_byte_at).max();
    match (first, last) {
        (Some(f), Some(l)) if l >= f => l - f,
        _ => Duration::ZERO,
    }
}

/// Duration of the steady-state measurement window: from the earliest
/// post-omit byte to the latest byte. Used for the Mbits/sec line when
/// `--omit` was requested.
pub fn measured_steady_state(receipts: &[StreamReceipt]) -> Duration {
    let first = receipts.iter().filter_map(|r| r.first_measured_at).min();
    let last = receipts.iter().filter_map(|r| r.last_byte_at).max();
    match (first, last) {
        (Some(f), Some(l)) if l >= f => l - f,
        _ => Duration::ZERO,
    }
}

/// Send ExchangeResults, receive the client's Results, then send ours.
/// The server builds its `Results` from the per-stream byte totals the
/// reader threads produced.
pub fn exchange_results(
    control: &mut Protocol,
    receipts: &[StreamReceipt],
    cpu: &CpuUsage,
) -> io::Result<()> {
    send_control_byte(control.transfer.as_mut(), Message::ExchangeResults)?;

    let _client_results: Results = recv_framed_json(control.transfer.as_mut())?;

    let server_results = build_server_results(receipts, cpu);
    send_framed_json(control.transfer.as_mut(), &server_results)?;
    Ok(())
}

/// Build the Results payload the server reports back. Per-stream bytes
/// come from the reader threads; UDP fields (jitter, lost, packets) are
/// populated from the receipt when available.
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

/// Spawn one thread per data stream, each draining its connection into
/// a byte counter. Returns the join handles so the caller can collect
/// per-stream totals after TestEnd. Each thread gets its own
/// IntervalReporter so per-stream rows print independently.
pub fn spawn_stream_readers(
    streams: Vec<Protocol>,
    omit: Duration,
) -> Vec<std::thread::JoinHandle<StreamReceipt>> {
    streams
        .into_iter()
        .enumerate()
        .map(|(i, mut protocol)| {
            let reporter = Some(IntervalReporter::new((i + 1) as u32, Duration::from_secs(1)));
            std::thread::spawn(move || recv_stream_bytes(&mut protocol, omit, reporter))
        })
        .collect()
}

/// Accept one TCP connection from `listener` and read the 37-byte cookie
/// the peer sends immediately after connecting. Returns the wrapped
/// `Protocol` along with the cookie bytes so a caller can match
/// subsequent data streams against it. The handshake timeout (if set)
/// is applied before the cookie read so a peer that connects and
/// never sends data cannot hang the server indefinitely.
pub fn accept_control(
    listener: &TcpListener,
    handshake_timeout: Option<Duration>,
) -> io::Result<(Protocol, [u8; COOKIE_LEN])> {
    let (stream, _addr) = listener.accept()?;
    let mut control = tcp_protocol(stream);
    control.transfer.set_read_timeout(handshake_timeout)?;
    let cookie = recv_cookie(control.transfer.as_mut())?;
    Ok((control, cookie))
}

/// Send `ParamExchange` to the client, then read the framed
/// `ClientOptions` JSON the client sends in reply.
pub fn negotiate_options(control: &mut Protocol) -> io::Result<ClientOptions> {
    send_control_byte(control.transfer.as_mut(), Message::ParamExchange)?;
    recv_framed_json::<ClientOptions>(control.transfer.as_mut())
}

/// Tell the client to open its data streams, then accept exactly `n`
/// additional connections on `listener` and verify each one sends the
/// same cookie as the control channel. Returns the accepted streams in
/// the order they arrived. The handshake timeout (if set) is applied
/// to the data socket while its cookie is read, then cleared so the
/// reader thread can block on the test workload without a spurious
/// timeout.
pub fn accept_streams(
    control: &mut Protocol,
    listener: &TcpListener,
    expected_cookie: &[u8; COOKIE_LEN],
    n: u32,
    handshake_timeout: Option<Duration>,
) -> io::Result<Vec<Protocol>> {
    send_control_byte(control.transfer.as_mut(), Message::CreateStreams)?;

    let mut streams = Vec::with_capacity(n as usize);
    for i in 0..n {
        let (stream, _addr) = listener.accept()?;
        let mut data = tcp_protocol(stream);
        data.transfer.set_read_timeout(handshake_timeout)?;
        let got = recv_cookie(data.transfer.as_mut())?;
        if &got != expected_cookie {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("stream {} cookie mismatch", i),
            ));
        }
        // Clear the deadline: the data stream now blocks on the actual
        // test payload, which must not be artificially capped.
        data.transfer.set_read_timeout(None)?;
        streams.push(data);
    }
    Ok(streams)
}

/// Notify the client that the test is starting and then running. These
/// are two separate 1-byte control messages — matching iPerf3's
/// sequence and what `client::handle_message_client` already expects.
pub fn send_test_start_running(control: &mut Protocol) -> io::Result<()> {
    send_control_byte(control.transfer.as_mut(), Message::TestStart)?;
    send_control_byte(control.transfer.as_mut(), Message::TestRunning)?;
    Ok(())
}

/// Drain `protocol` into the bit bucket until the peer closes or an
/// error occurs. Records the timestamps of the first and last bytes so
/// the server can report real measured throughput. Bytes received
/// within `omit` of the first byte are attributed to the omit window
/// and excluded from the measurement totals.
pub fn recv_stream_bytes(
    protocol: &mut Protocol,
    omit: Duration,
    mut reporter: Option<IntervalReporter>,
) -> StreamReceipt {
    let mut buf = [0u8; crate::common::wire::DEFAULT_TCP_LEN];
    let mut receipt = StreamReceipt::empty();
    loop {
        match protocol.transfer.recv(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let now = Instant::now();
                if receipt.first_byte_at.is_none() {
                    receipt.first_byte_at = Some(now);
                }
                receipt.last_byte_at = Some(now);
                receipt.bytes += n as u64;

                let first = receipt.first_byte_at.expect("first_byte_at just set");
                let in_omit = now.duration_since(first) < omit;
                if in_omit {
                    receipt.bytes_omit += n as u64;
                } else if receipt.first_measured_at.is_none() {
                    receipt.first_measured_at = Some(now);
                }

                if let Some(r) = reporter.as_mut() {
                    if let Some(snap) = r.on_bytes(n as u64, now) {
                        println!(
                            "[SRV {}] {:.2}-{:.2} sec  {} bytes  {:.2} Mbits/sec",
                            snap.stream_id,
                            snap.start_sec,
                            snap.end_sec,
                            snap.bytes,
                            snap.mbits_per_sec(),
                        );
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }

    if let Some(mut r) = reporter {
        if let Some(snap) = receipt.last_byte_at.and_then(|t| r.flush(t)) {
            if snap.bytes > 0 {
                println!(
                    "[SRV {}] {:.2}-{:.2} sec  {} bytes  {:.2} Mbits/sec (final)",
                    snap.stream_id,
                    snap.start_sec,
                    snap.end_sec,
                    snap.bytes,
                    snap.mbits_per_sec(),
                );
            }
        }
    }

    receipt
}

/// Wrap a blocking `TcpStream` in our `Protocol` abstraction. Kept
/// centralized so every accepted connection goes through the same
/// construction path (and so test code can reuse it).
pub fn tcp_protocol(stream: TcpStream) -> Protocol {
    Protocol {
        transfer: Box::new(Tcp::new(stream)),
    }
}

fn cookie_display(cookie: &[u8; COOKIE_LEN]) -> String {
    // The first 36 bytes are ASCII alphanumerics; the last is NUL.
    String::from_utf8_lossy(&cookie[..COOKIE_LEN - 1]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::cookie::generate_cookie;
    use crate::common::wire::{send_framed_json, DEFAULT_TCP_LEN};
    use std::io::Write;
    use std::net::TcpStream;
    use std::thread;

    /// Bind a `TcpListener` on an ephemeral loopback port and return it
    /// along with its concrete address so the test client can reach it.
    fn bind_loopback() -> (TcpListener, std::net::SocketAddr) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
        let addr = listener.local_addr().expect("local_addr");
        (listener, addr)
    }

    #[test]
    fn accepts_tcp_connection_and_reads_37_byte_cookie() {
        let (listener, addr) = bind_loopback();
        let cookie = generate_cookie();

        let client = thread::spawn(move || {
            let mut stream = TcpStream::connect(addr).expect("connect");
            stream.write_all(&cookie).expect("send cookie");
            // Keep the connection open so the accept side doesn't race EOF.
            stream
        });

        let (_control, received) = accept_control(&listener, None).expect("accept");
        assert_eq!(received, cookie);

        drop(client.join().expect("client thread"));
    }

    #[test]
    fn negotiate_options_sends_param_exchange_then_parses_options() {
        let (listener, addr) = bind_loopback();
        let cookie = generate_cookie();
        let expected = ClientOptions::tcp_defaults(4, 2, DEFAULT_TCP_LEN as u32);

        let client_opts = expected.clone();
        let client = thread::spawn(move || {
            let stream = TcpStream::connect(addr).expect("connect");
            let mut proto = tcp_protocol(stream);

            // 1. Send cookie.
            proto
                .transfer
                .send(&cookie)
                .expect("client send cookie");

            // 2. Read the single ParamExchange byte.
            let mut byte = [0u8; 1];
            let n = proto.transfer.recv(&mut byte).expect("client recv byte");
            assert_eq!(n, 1);
            assert_eq!(byte[0], Message::ParamExchange as u8);

            // 3. Send options JSON back.
            send_framed_json(proto.transfer.as_mut(), &client_opts).expect("client send opts");
        });

        let (mut control, _cookie) = accept_control(&listener, None).expect("accept");
        let opts = negotiate_options(&mut control).expect("negotiate");
        assert_eq!(opts, expected);

        client.join().expect("client thread");
    }

    #[test]
    fn accept_streams_returns_n_protocols_with_matching_cookie() {
        let (listener, addr) = bind_loopback();
        let cookie = generate_cookie();
        let parallel: u32 = 3;

        // Client: opens 1 control + 3 data connections, all sending the same
        // cookie. Reads ParamExchange + CreateStreams to keep the control
        // channel drained.
        let client_cookie = cookie;
        let client = thread::spawn(move || {
            let mut ctrl = TcpStream::connect(addr).expect("ctrl connect");
            ctrl.write_all(&client_cookie).expect("ctrl cookie");

            // Drain ParamExchange from the server.
            let mut byte = [0u8; 1];
            std::io::Read::read_exact(&mut ctrl, &mut byte).expect("ctrl recv pe");
            assert_eq!(byte[0], Message::ParamExchange as u8);

            // Send options via the Socket helper around the stream.
            let mut ctrl_proto = tcp_protocol(ctrl);
            send_framed_json(
                ctrl_proto.transfer.as_mut(),
                &ClientOptions::tcp_defaults(1, parallel, DEFAULT_TCP_LEN as u32),
            )
            .expect("send opts");

            // Expect CreateStreams on the control channel.
            let mut byte = [0u8; 1];
            ctrl_proto.transfer.recv(&mut byte).expect("ctrl recv cs");
            assert_eq!(byte[0], Message::CreateStreams as u8);

            // Open N data streams, each with the same cookie. Hold them
            // until the server returns; _data_conns is intentionally
            // retained in-thread (Protocol is !Send, so it cannot be
            // returned from a thread closure).
            let mut _data_conns: Vec<TcpStream> = Vec::new();
            for _ in 0..parallel {
                let mut s = TcpStream::connect(addr).expect("data connect");
                s.write_all(&client_cookie).expect("data cookie");
                _data_conns.push(s);
            }

            // Keep the sockets alive until the test's assertions run.
            std::thread::sleep(std::time::Duration::from_millis(100));
        });

        let (mut control, got_cookie) = accept_control(&listener, None).expect("accept control");
        assert_eq!(got_cookie, cookie);

        let opts = negotiate_options(&mut control).expect("negotiate");
        assert_eq!(opts.parallel, parallel);

        let streams =
            accept_streams(&mut control, &listener, &cookie, opts.parallel, None).expect("streams");
        assert_eq!(streams.len(), parallel as usize);

        client.join().expect("client thread");
    }

    #[test]
    fn accept_streams_rejects_mismatched_cookie() {
        let (listener, addr) = bind_loopback();
        let good = generate_cookie();
        let mut bad = good;
        bad[0] = if bad[0] == b'A' { b'B' } else { b'A' };

        let client_cookie = good;
        let bad_cookie = bad;
        let client = thread::spawn(move || {
            let mut ctrl = TcpStream::connect(addr).expect("ctrl connect");
            ctrl.write_all(&client_cookie).expect("ctrl cookie");

            let mut byte = [0u8; 1];
            std::io::Read::read_exact(&mut ctrl, &mut byte).expect("ctrl recv pe");

            let mut ctrl_proto = tcp_protocol(ctrl);
            send_framed_json(
                ctrl_proto.transfer.as_mut(),
                &ClientOptions::tcp_defaults(1, 1, DEFAULT_TCP_LEN as u32),
            )
            .expect("send opts");

            let mut byte = [0u8; 1];
            ctrl_proto.transfer.recv(&mut byte).expect("ctrl recv cs");

            // Data stream with the WRONG cookie.
            let mut s = TcpStream::connect(addr).expect("data connect");
            s.write_all(&bad_cookie).expect("data bad cookie");

            std::thread::sleep(std::time::Duration::from_millis(100));
        });

        let (mut control, _cookie) = accept_control(&listener, None).expect("accept control");
        let _ = negotiate_options(&mut control).expect("negotiate");

        let err = match accept_streams(&mut control, &listener, &good, 1, None) {
            Ok(_) => panic!("expected cookie mismatch error"),
            Err(e) => e,
        };
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);

        let _ = client.join();
    }

    #[test]
    fn recv_stream_bytes_sums_until_peer_closes() {
        let (listener, addr) = bind_loopback();
        const CHUNKS: usize = 4;
        const CHUNK_SIZE: usize = 8192;
        let expected = (CHUNKS * CHUNK_SIZE) as u64;

        let writer = thread::spawn(move || {
            let mut s = TcpStream::connect(addr).expect("writer connect");
            let chunk = vec![0xABu8; CHUNK_SIZE];
            for _ in 0..CHUNKS {
                s.write_all(&chunk).expect("write chunk");
            }
            // Drop s -> EOF on the receiver.
        });

        let (stream, _addr) = listener.accept().expect("accept");
        let mut proto = tcp_protocol(stream);
        let receipt = recv_stream_bytes(&mut proto, Duration::ZERO, None);

        writer.join().expect("writer");
        assert_eq!(receipt.bytes, expected);
        assert!(receipt.first_byte_at.is_some(), "first byte timestamp should be set");
        assert!(receipt.last_byte_at.is_some(), "last byte timestamp should be set");
        // Elapsed must be non-negative and bounded above by a sanity ceiling.
        let elapsed = receipt.elapsed();
        assert!(elapsed < Duration::from_secs(5), "unexpectedly long: {:?}", elapsed);
    }

    #[test]
    fn recv_stream_bytes_is_zero_on_immediate_close() {
        let (listener, addr) = bind_loopback();

        let writer = thread::spawn(move || {
            let _ = TcpStream::connect(addr).expect("connect then drop");
            // immediate drop -> peer closes with no payload
        });

        let (stream, _addr) = listener.accept().expect("accept");
        let mut proto = tcp_protocol(stream);
        let receipt = recv_stream_bytes(&mut proto, Duration::ZERO, None);

        writer.join().expect("writer");
        assert_eq!(receipt.bytes, 0);
        assert!(receipt.first_byte_at.is_none());
        assert!(receipt.last_byte_at.is_none());
        assert_eq!(receipt.elapsed(), Duration::ZERO);
    }

    #[test]
    fn spawn_stream_readers_returns_handle_per_stream() {
        let (listener, addr) = bind_loopback();
        let n = 2;
        const BYTES_PER_STREAM: usize = 1024;

        // Two writer threads, each sending a fixed payload then closing.
        let writers: Vec<_> = (0..n)
            .map(|_| {
                thread::spawn(move || {
                    let mut s = TcpStream::connect(addr).expect("connect");
                    s.write_all(&vec![7u8; BYTES_PER_STREAM]).expect("write");
                })
            })
            .collect();

        let mut accepted = Vec::new();
        for _ in 0..n {
            let (stream, _) = listener.accept().expect("accept");
            accepted.push(tcp_protocol(stream));
        }

        let handles = spawn_stream_readers(accepted, Duration::ZERO);
        assert_eq!(handles.len(), n);
        let mut total: u64 = 0;
        for h in handles {
            total += h.join().unwrap().bytes;
        }
        assert_eq!(total, (n * BYTES_PER_STREAM) as u64);

        for w in writers {
            w.join().expect("writer");
        }
    }

    #[test]
    fn send_test_start_running_emits_two_bytes_in_order() {
        use crate::common::protocol::testing::MockSocket;

        let mock = MockSocket::new();
        let written = mock.written.clone();
        let mut control = Protocol {
            transfer: Box::new(mock),
        };

        send_test_start_running(&mut control).expect("send");
        let buf = written.lock().unwrap().clone();

        assert_eq!(buf, vec![Message::TestStart as u8, Message::TestRunning as u8]);
    }

    #[test]
    fn wait_for_test_end_returns_on_test_end_byte() {
        use crate::common::protocol::testing::MockSocket;

        let mock = MockSocket::new();
        mock.recv_queue
            .lock()
            .unwrap()
            .push(vec![Message::TestEnd as u8]);
        let mut control = Protocol {
            transfer: Box::new(mock),
        };

        wait_for_test_end(&mut control).expect("ok");
    }

    #[test]
    fn wait_for_test_end_skips_unexpected_bytes() {
        use crate::common::protocol::testing::MockSocket;

        let mock = MockSocket::new();
        mock.recv_queue
            .lock()
            .unwrap()
            .push(vec![Message::TestRunning as u8]);
        mock.recv_queue
            .lock()
            .unwrap()
            .push(vec![Message::TestEnd as u8]);
        let mut control = Protocol {
            transfer: Box::new(mock),
        };

        wait_for_test_end(&mut control).expect("ok");
    }

    fn receipt_with(
        bytes: u64,
        first: Option<Instant>,
        last: Option<Instant>,
    ) -> StreamReceipt {
        StreamReceipt {
            bytes,
            bytes_omit: 0,
            first_byte_at: first,
            first_measured_at: first,
            last_byte_at: last,
            jitter_ms: 0.0,
            lost: 0,
            ooo: 0,
            packets: 0,
        }
    }

    #[test]
    fn build_server_results_populates_one_stream_per_total() {
        let receipts = [receipt_with(1_000, None, None), receipt_with(2_500, None, None)];
        let r = build_server_results(&receipts, &CpuUsage::ZERO);
        assert_eq!(r.streams.len(), 2);
        assert_eq!(r.streams[0].id, 1);
        assert_eq!(r.streams[0].bytes, 1_000);
        assert_eq!(r.streams[1].id, 2);
        assert_eq!(r.streams[1].bytes, 2_500);
    }

    #[test]
    fn measured_duration_spans_earliest_first_to_latest_last() {
        let base = Instant::now();
        let r1 = receipt_with(10, Some(base), Some(base + Duration::from_millis(800)));
        let r2 = receipt_with(
            20,
            Some(base + Duration::from_millis(100)),
            Some(base + Duration::from_millis(1_000)),
        );

        assert_eq!(measured_duration(&[r1, r2]), Duration::from_millis(1_000));
    }

    #[test]
    fn recv_stream_bytes_attributes_early_bytes_to_omit() {
        let (listener, addr) = bind_loopback();

        let writer = thread::spawn(move || {
            let mut s = TcpStream::connect(addr).expect("writer connect");
            // Send some early bytes, sleep past the omit window, send more.
            s.write_all(&[0u8; 1024]).expect("early write");
            thread::sleep(Duration::from_millis(120));
            s.write_all(&[0u8; 2048]).expect("late write");
        });

        let (stream, _) = listener.accept().expect("accept");
        let mut proto = tcp_protocol(stream);
        let receipt = recv_stream_bytes(&mut proto, Duration::from_millis(80), None);

        writer.join().expect("writer");

        assert_eq!(receipt.bytes, 3072);
        assert!(receipt.bytes_omit > 0, "expected some bytes to land in omit window");
        assert!(
            receipt.bytes_measured() > 0,
            "expected some bytes to land in the measurement window"
        );
        assert!(receipt.first_measured_at.is_some());
    }

    #[test]
    fn measured_duration_is_zero_when_no_bytes_observed() {
        assert_eq!(
            measured_duration(&[StreamReceipt::empty(), StreamReceipt::empty()]),
            Duration::ZERO
        );
    }

    #[test]
    fn stream_receipt_elapsed_is_zero_without_bytes() {
        assert_eq!(StreamReceipt::empty().elapsed(), Duration::ZERO);
    }

    #[test]
    fn finalize_test_sends_display_results_then_waits_for_iperf_done() {
        use crate::common::protocol::testing::MockSocket;

        let mock = MockSocket::new();
        let written = mock.written.clone();
        mock.recv_queue
            .lock()
            .unwrap()
            .push(vec![Message::IperfDone as u8]);

        let mut control = Protocol {
            transfer: Box::new(mock),
        };

        finalize_test(&mut control).expect("ok");
        let buf = written.lock().unwrap().clone();
        assert_eq!(buf, vec![Message::DisplayResults as u8]);
    }

    #[test]
    fn finalize_test_tolerates_extra_bytes_before_iperf_done() {
        use crate::common::protocol::testing::MockSocket;

        let mock = MockSocket::new();
        mock.recv_queue
            .lock()
            .unwrap()
            .push(vec![Message::TestRunning as u8]);
        mock.recv_queue
            .lock()
            .unwrap()
            .push(vec![Message::IperfDone as u8]);

        let mut control = Protocol {
            transfer: Box::new(mock),
        };
        finalize_test(&mut control).expect("ok");
    }

    #[test]
    fn format_summary_contains_bytes_and_throughput() {
        let line = format_summary(10_000_000, std::time::Duration::from_secs(1), 1);
        assert!(line.contains("10000000"));
        assert!(line.contains("stream"));
        assert!(line.contains("Mbits/sec"));
    }

    #[test]
    fn format_summary_handles_zero_duration_without_panic() {
        // Just make sure we don't divide by zero.
        let _ = format_summary(1_000, std::time::Duration::from_secs(0), 1);
    }

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

    #[test]
    fn server_receipt_empty_zeros_udp_fields() {
        let r = StreamReceipt::empty();
        assert_eq!(r.jitter_ms, 0.0);
        assert_eq!(r.lost, 0);
        assert_eq!(r.ooo, 0);
        assert_eq!(r.packets, 0);
    }

    #[test]
    fn exchange_results_sends_server_results_with_totals() {
        use crate::common::protocol::testing::MockSocket;

        let mock = MockSocket::new();
        let written = mock.written.clone();

        // Seed the client-side Results the server expects to receive.
        let client_results = Results::empty();
        let client_bytes = serde_json::to_vec(&client_results).unwrap();
        let len_prefix = (client_bytes.len() as u32).to_be_bytes().to_vec();
        {
            let mut q = mock.recv_queue.lock().unwrap();
            q.push(len_prefix);
            q.push(client_bytes);
        }

        let mut control = Protocol {
            transfer: Box::new(mock),
        };
        let receipts = [
            receipt_with(4096, None, None),
            receipt_with(8192, None, None),
        ];
        exchange_results(&mut control, &receipts, &CpuUsage::ZERO).expect("exchange");

        let buf = written.lock().unwrap().clone();
        // First byte: ExchangeResults, then 4-byte BE length, then body.
        assert_eq!(buf[0], Message::ExchangeResults as u8);
        let body_len = u32::from_be_bytes(buf[1..5].try_into().unwrap()) as usize;
        let body = &buf[5..5 + body_len];
        let sent: Results = serde_json::from_slice(body).unwrap();
        assert_eq!(sent.streams.len(), 2);
        assert_eq!(sent.streams[0].bytes, 4096);
        assert_eq!(sent.streams[1].bytes, 8192);
    }
}
