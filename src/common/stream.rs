use std::sync::mpsc;
use std::{thread, time::{Duration, Instant}};

use super::cookie::COOKIE_LEN;
use super::interval::IntervalReporter;
use super::test::ClientStreamReceipt;
use super::{connect, protocol::Protocol, test::Test, timer::Timer, Message};

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
                                    "[CLI {}] {:.2}-{:.2} sec  {} bytes  {:.2} Mbits/sec",
                                    snap.stream_id,
                                    snap.start_sec,
                                    snap.end_sec,
                                    snap.bytes,
                                    snap.mbits_per_sec(),
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
                                "[CLI {}] {:.2}-{:.2} sec  {} bytes  {:.2} Mbits/sec (final)",
                                snap.stream_id,
                                snap.start_sec,
                                snap.end_sec,
                                snap.bytes,
                                snap.mbits_per_sec(),
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
}
