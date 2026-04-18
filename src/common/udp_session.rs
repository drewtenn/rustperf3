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

pub fn bind_udp(addr: &str, handshake_timeout: Option<Duration>) -> io::Result<UdpSocket> {
    let socket = UdpSocket::bind(addr)?;
    socket.set_read_timeout(handshake_timeout)?;
    Ok(socket)
}

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

pub fn run_udp_receiver(
    socket: UdpSocket,
    stream_order: Vec<SocketAddr>,
    omit: Duration,
    stop_flag: Arc<AtomicBool>,
) -> Vec<StreamReceipt> {
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
            let _ = socket.set_read_timeout(Some(Duration::from_millis(50)));
        }

        match socket.recv_from(&mut buf) {
            Ok((len, src)) => {
                if len < UDP_HEADER_LEN {
                    continue;
                }
                let Some(state) = states.get_mut(&src) else {
                    continue;
                };
                let hdr = match UdpHeader::decode(&buf[..UDP_HEADER_LEN]) {
                    Ok(h) => h,
                    Err(_) => continue,
                };
                let now = Instant::now();

                if hdr.is_sentinel() {
                    state.saw_sentinel = true;
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
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock
                    || e.kind() == io::ErrorKind::TimedOut =>
            {
                if stop_flag.load(Ordering::Relaxed) {
                    break;
                }
                continue;
            }
            Err(_) => break,
        }
    }

    stream_order
        .into_iter()
        .map(|addr| {
            states
                .remove(&addr)
                .map(|s| s.receipt)
                .unwrap_or_else(StreamReceipt::empty)
        })
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

        let sender = thread::spawn(move || {
            let sock = UdpSocket::bind("127.0.0.1:0").expect("client bind");
            sock.connect(server_addr).expect("client connect");
            sock.send(&cookie).expect("send cookie");

            for seq in 0i64..5 {
                let mut buf = [0u8; 64];
                UdpHeader { tv_sec: 0, tv_usec: 0, seq }
                    .encode(&mut buf[..UDP_HEADER_LEN])
                    .unwrap();
                sock.send(&buf).expect("send data");
                thread::sleep(Duration::from_millis(5));
            }
            let mut buf = [0u8; UDP_HEADER_LEN];
            UdpHeader { tv_sec: 0, tv_usec: 0, seq: -1 }.encode(&mut buf).unwrap();
            sock.send(&buf).expect("send sentinel");
        });

        let stream_addrs = accept_udp_streams(&server, &cookie, 1).expect("accept");
        assert_eq!(stream_addrs.len(), 1);

        let stop = Arc::new(AtomicBool::new(true));
        let receipts = run_udp_receiver(server, stream_addrs, Duration::ZERO, stop);
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].packets, 5);
        assert_eq!(receipts[0].bytes, 5 * 64);
        assert_eq!(receipts[0].lost, 0);
        assert_eq!(receipts[0].ooo, 0);

        let _ = sender.join();
    }
}
