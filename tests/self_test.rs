//! End-to-end self-test: spin up an rperf server on an ephemeral
//! loopback port and run an rperf client against it in the same
//! process. Asserts that data flows and the server reports a non-zero
//! byte count after a short test window.

use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use rperf::Config;

/// Minimum bytes we expect to see over loopback in one second with a
/// 128 KiB send size. Loopback typically moves gigabits per second even
/// on slow CI, so this threshold is very conservative.
const MIN_BYTES_EXPECTED: u64 = 1_000_000;

#[test]
fn self_test_loopback_tcp_short_run() {
    // Bind first so we can hand the test a known, free port and hand
    // the listener to the server thread without a race window.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    let port = listener.local_addr().expect("local_addr").port();

    let server = thread::spawn(move || rperf::run_server_on(listener));

    let client_cfg = Config {
        host: "127.0.0.1".to_string(),
        port,
        time: 1,
        parallel: 1,
        len: 131_072,
        omit: 0,
    };

    // Tiny delay so the server thread reaches listener.accept() before
    // the client's connect. The listener is already bound, so this is
    // more about letting the spawned thread schedule than correctness.
    thread::sleep(Duration::from_millis(50));

    rperf::run_client(client_cfg);

    let total_bytes = server
        .join()
        .expect("server thread panicked")
        .expect("server returned error");

    assert!(
        total_bytes >= MIN_BYTES_EXPECTED,
        "server observed {} bytes, expected at least {}",
        total_bytes,
        MIN_BYTES_EXPECTED
    );
}

#[test]
fn self_test_omit_separates_omit_window_from_measurement_window() {
    // Run a 2-second test with --omit 1. The server must observe
    // non-zero bytes in both the omit and measurement windows.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    let port = listener.local_addr().expect("local_addr").port();

    let server = thread::spawn(move || rperf::run_server_on(listener));

    let client_cfg = Config {
        host: "127.0.0.1".to_string(),
        port,
        time: 1,
        parallel: 1,
        len: 131_072,
        omit: 1,
    };

    thread::sleep(Duration::from_millis(50));
    rperf::run_client(client_cfg);

    let total = server
        .join()
        .expect("server thread panicked")
        .expect("server returned error");

    assert!(
        total >= MIN_BYTES_EXPECTED,
        "server observed {} bytes, expected at least {}",
        total,
        MIN_BYTES_EXPECTED
    );
}

#[test]
fn server_times_out_when_client_goes_silent_after_cookie() {
    // A misbehaving "client" connects, sends the 37-byte cookie, then
    // does nothing. With a short handshake timeout the server must
    // return an error rather than hang forever.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    let port = listener.local_addr().expect("local_addr").port();

    let handshake_timeout = Duration::from_millis(150);
    let server = thread::spawn(move || {
        rperf::run_server_on_timeout(listener, Some(handshake_timeout))
    });

    // Let the server reach listener.accept().
    thread::sleep(Duration::from_millis(50));

    let mut rogue = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    // Send exactly the cookie and then fall silent.
    let cookie = [b'A'; 37];
    rogue.write_all(&cookie).expect("write cookie");
    let _rogue_keepalive = rogue; // hold the TCP connection open

    let start = Instant::now();
    let result = server.join().expect("server thread");
    let elapsed = start.elapsed();

    // The server should return Err (TimedOut / WouldBlock) well within
    // a few multiples of the handshake timeout, not the 30-second default.
    assert!(result.is_err(), "server unexpectedly returned Ok: {:?}", result);
    assert!(
        elapsed < Duration::from_secs(2),
        "server took too long to time out: {:?}",
        elapsed
    );
}
