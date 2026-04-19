use std::net::UdpSocket;
use std::sync::mpsc;
use std::{thread, time::{Duration, Instant}};

use super::cookie::COOKIE_LEN;
use super::format;
use super::interval::IntervalReporter;
use super::pacing::TokenBucket;
use super::test::ClientStreamReceipt;
use super::udp_header::{UdpHeader, UDP_HEADER_LEN};
use super::{connect, protocol::Protocol, test::Test, timer::Timer, Message};

/// One-line iPerf3-compatible header for the interval table printed at
/// the start of a test run.
pub const INTERVAL_HEADER: &str = "[ ID] Interval           Transfer     Bitrate";

/// 49-column dashed separator iPerf3 prints between the per-interval
/// rows and the summary rows.
pub const SUMMARY_SEPARATOR: &str = "- - - - - - - - - - - - - - - - - - - - - - - - -";

/// Render one interval row the way iPerf3 does, e.g.
/// `[  5]   0.00-1.00   sec  128 MBytes   1.07 Gbits/sec`.
pub fn format_interval_row(stream_id: u32, start_sec: f64, end_sec: f64, bytes: u64) -> String {
    let secs = (end_sec - start_sec).max(0.000_001);
    let bitrate = (bytes as f64 * 8.0) / secs;
    format!(
        "[{:>3}] {:>6.2}-{:<5.2} sec  {:>10}  {:>14}",
        stream_id,
        start_sec,
        end_sec,
        format::bytes(bytes),
        format::bitrate_bps(bitrate),
    )
}

/// Summary row with a trailing role suffix (`sender` or `receiver`),
/// matching iPerf3's end-of-test output.
pub fn format_summary_row(
    stream_id: u32,
    start_sec: f64,
    end_sec: f64,
    bytes: u64,
    role: &str,
) -> String {
    format!("{}  {}", format_interval_row(stream_id, start_sec, end_sec, bytes), role)
}

#[derive(Default)]
pub struct Stream {
    pub protocol: Option<Protocol>,
}

/// Allocate the per-send payload of the requested length. Contents are
/// deterministic but irrelevant to iPerf3 accounting; the server only
/// weighs the byte count.
pub fn build_payload(len: usize) -> Vec<u8> {
    vec![1u8; len]
}

/// Returns true when the timer has been running at least `duration`.
/// Isolated so the streaming loop can be unit tested without a thread.
pub fn should_stop(timer: &Timer, duration: Duration) -> bool {
    timer.is_elapsed(duration)
}

impl Stream {
    pub fn new() -> Self {
        Self { protocol: None }
    }

    pub fn start(test: &Test, host: String, cookie: [u8; COOKIE_LEN], stream_id: u32) {
        let tx = test.tx_channel.clone();
        let receipt_tx = test.receipt_tx.clone();
        let len = test.config.len as usize;
        // Total wall-clock budget for this stream = omit + time. iPerf3
        // treats omit as *additional* seconds on top of the measurement
        // window; rperf does the same so cfg.time always reflects the
        // reported number.
        let duration = Duration::from_secs((test.config.time + test.config.omit) as u64);
        let omit = Duration::from_secs(test.config.omit as u64);

        thread::spawn(move || {
            let mut receipt = ClientStreamReceipt::empty(stream_id);
            let mut reporter = IntervalReporter::new(stream_id, Duration::from_secs(1));
            let mut stream = Stream::new();

            if let Some(mut protocol) = connect(host, &cookie) {
                // Data streams are blocking so the write loop gets TCP
                // back-pressure instead of a flood of WouldBlock errors.
                if let Err(e) = protocol.transfer.set_nonblocking(false) {
                    eprintln!("failed to set data stream to blocking: {:?}", e);
                    emit_stream_finished(&receipt_tx, receipt, &tx);
                    return;
                }
                stream.protocol = Some(protocol);

                let timer = Timer::new();
                let tx_buffer = build_payload(len);

                while !should_stop(&timer, duration) {
                    match stream.send_data(&tx_buffer) {
                        Ok(n) if n > 0 => {
                            let now = Instant::now();
                            if receipt.first_send_at.is_none() {
                                receipt.first_send_at = Some(now);
                            }
                            receipt.last_send_at = Some(now);
                            receipt.bytes_sent += n as u64;

                            let first = receipt
                                .first_send_at
                                .expect("first_send_at just set");
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
                        Ok(_) => continue,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                        Err(e) => {
                            eprintln!("stream send error: {:?}", e);
                            break;
                        }
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

                // Pull the kernel's retransmit count before the socket
                // drops. On non-Linux this returns Unsupported; we
                // treat that as zero rather than bailing.
                if let Some(proto) = stream.protocol.as_ref() {
                    receipt.retransmits = proto.transfer.tcp_retransmits().unwrap_or(0);
                }
            }

            emit_stream_finished(&receipt_tx, receipt, &tx);
        });
    }

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

            if let Err(e) = socket.send(&cookie) {
                eprintln!("udp cookie send failed: {:?}", e);
                emit_stream_finished(&receipt_tx, receipt, &tx);
                return;
            }

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

                        let first = receipt.first_send_at.expect("set above");
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

            // Sentinel packet: seq = -1, minimal payload (header only).
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

    pub fn send_data(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self.protocol {
            Some(ref mut protocol) => protocol.transfer.send(buf),
            None => Ok(0),
        }
    }
}

/// Fire a finished-receipt on the receipt channel and an end-of-test
/// signal on the control channel. Both are fatal-log-and-continue on
/// failure because by this point the test is already winding down and
/// the main loop may have already exited.
fn emit_stream_finished(
    receipt_tx: &mpsc::Sender<ClientStreamReceipt>,
    receipt: ClientStreamReceipt,
    tx: &mpsc::Sender<Message>,
) {
    if let Err(e) = receipt_tx.send(receipt) {
        eprintln!("failed to send stream receipt to client loop: {:?}", e);
    }
    if let Err(e) = tx.send(Message::TestEnd) {
        eprintln!("failed to notify client loop of TestEnd: {:?}", e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_payload_matches_len() {
        let buf = build_payload(65_536);
        assert_eq!(buf.len(), 65_536);
        assert!(buf.iter().all(|b| *b == 1));
    }

    #[test]
    fn build_payload_zero_len_is_empty() {
        assert!(build_payload(0).is_empty());
    }

    #[test]
    fn should_stop_is_false_before_duration() {
        let timer = Timer::new();
        assert!(!should_stop(&timer, Duration::from_secs(60)));
    }

    #[test]
    fn should_stop_is_true_after_duration() {
        let timer = Timer::new();
        std::thread::sleep(Duration::from_millis(20));
        assert!(should_stop(&timer, Duration::from_millis(5)));
    }

    #[test]
    fn send_data_without_protocol_is_zero_bytes() {
        let mut stream = Stream::new();
        let n = stream.send_data(b"hello").expect("no-protocol send is Ok");
        assert_eq!(n, 0);
    }

    #[test]
    fn send_data_forwards_bytes_to_protocol() {
        use crate::common::protocol::testing::MockSocket;

        let mock = MockSocket::new();
        let written = mock.written.clone();
        let mut stream = Stream::new();
        stream.protocol = Some(Protocol {
            transfer: Box::new(mock),
        });

        let payload = b"abcdef";
        let n = stream.send_data(payload).expect("send ok");
        assert_eq!(n, payload.len());
        assert_eq!(&*written.lock().unwrap(), payload);
    }

    #[test]
    fn send_data_propagates_err_from_protocol() {
        use crate::common::protocol::testing::MockSocket;
        use std::io::{Error, ErrorKind};

        let mock = MockSocket::new().with_send_error(Error::other("boom"));
        let mut stream = Stream::new();
        stream.protocol = Some(Protocol {
            transfer: Box::new(mock),
        });

        let err = stream.send_data(b"payload").expect_err("should bubble");
        assert_eq!(err.kind(), ErrorKind::Other);
    }

    #[test]
    fn start_udp_sends_cookie_then_data_then_sentinel() {
        use crate::common::cookie::COOKIE_LEN;
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
        cfg.len = 256;
        cfg.transport = TransportKind::Udp;
        // 100 Kbps → ~49 packets/s → ~49 total in 1 s, well within the
        // 200-iteration drain loop below.
        cfg.bandwidth = 100_000;

        let test = Test::new(cfg);
        let cookie = test.cookie;
        let host = test.config.host_port();

        Stream::start_udp(&test, host, cookie, 1);

        // First datagram: cookie (37 bytes).
        let mut buf = [0u8; 2048];
        let (n, _src) = server.recv_from(&mut buf).expect("recv cookie");
        assert_eq!(n, COOKIE_LEN);
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
}
