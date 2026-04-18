use crate::common::cpu::{self, CpuUsage};
use crate::common::stream::Stream;
use crate::common::test::{Config, Test};
use crate::common::wire::{
    self, recv_control_byte, recv_framed_json, send_control_byte, send_framed_json, ClientOptions,
    Results, StreamResults,
};
use crate::common::{connect, Message};

pub fn run_client(config: Config) {
    let mut test = Test::new(config);
    let host_port = test.config.host_port();
    test.control_channel = connect(host_port, &test.cookie);
    test.cpu_start = Some(cpu::sample());

    client_loop(&mut test);
}

fn client_recv(test: &mut Test) -> bool {
    if let Some(protocol) = &mut test.control_channel {
        match recv_control_byte(protocol.transfer.as_mut()) {
            Ok(message) => return handle_message_client(test, message),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => eprintln!("control recv error: {:?}", e),
        }
    }
    false
}

fn client_loop(test: &mut Test) {
    // One TestEnd (on the control channel) is enough no matter how
    // many stream threads finish — iPerf3's state machine only expects
    // a single transition. Streams count themselves off here.
    let mut streams_finished: u32 = 0;
    let mut test_end_sent = false;

    loop {
        if client_recv(test) {
            break;
        }

        // Drain any finished-stream receipts as they arrive so they're
        // ready when ExchangeResults fires.
        test.drain_receipts();

        if let Ok(message) = test.rx_channel.try_recv() {
            match message {
                Message::TestEnd => {
                    streams_finished += 1;
                    if streams_finished >= test.config.parallel.max(1) && !test_end_sent {
                        if let Some(protocol) = test.control_channel.as_mut() {
                            log_send(
                                wire::send_control_byte(
                                    protocol.transfer.as_mut(),
                                    Message::TestEnd,
                                ),
                                "Test end sent.",
                                "TestEnd",
                            );
                            test_end_sent = true;
                        }
                    }
                }
                _ => eprintln!("Unknown message from stream thread: {:?}", message),
            }
        }
    }

    // Final drain to make sure nothing arrived between the last
    // try_recv and the loop-exiting control message.
    test.drain_receipts();
}

fn handle_message_client(test: &mut Test, message: Message) -> bool {
    let mut is_done = false;

    match message {
        Message::ParamExchange => {
            println!("Received parameter exchange control message.");
            send_options(test);
        }

        Message::CreateStreams => {
            println!("Received create streams control message.");
            create_streams(test);
        }

        Message::TestStart => {
            test.is_started = true;
            println!("Received test start control message.")
        }

        Message::TestRunning => {
            test.is_running = true;
            println!("Received test running control message.")
        }

        Message::ExchangeResults => {
            println!("Received exchange results control message.");
            exchange_results(test);
        }

        Message::DisplayResults => {
            println!("Received display results control message.");
            send_iperf_done(test);
            is_done = true;
        }

        Message::IperfDone => {
            println!("Received done control message.");
            is_done = true;
        }

        _ => eprintln!("Received unknown control message."),
    }

    is_done
}

fn exchange_results(test: &mut Test) {
    // Make sure every finished-stream receipt is accounted for before
    // we build our Results payload.
    test.drain_receipts();
    let cpu_usage = match test.cpu_start {
        Some(start) => {
            let now = cpu::sample();
            let wall = client_session_duration(&test.receipts)
                .unwrap_or(std::time::Duration::ZERO);
            cpu::usage(&start, &now, wall)
        }
        None => CpuUsage::ZERO,
    };
    if cpu_usage.total_pct > 0.0 {
        println!(
            "[CLIENT] CPU: {:.2}% total ({:.2}% user, {:.2}% system)",
            cpu_usage.total_pct, cpu_usage.user_pct, cpu_usage.system_pct,
        );
    }
    let client_results = build_client_results(&test.receipts, &cpu_usage);

    if let Some(duration) = client_session_duration(&test.receipts) {
        let total: u64 = test.receipts.iter().map(|r| r.bytes_sent).sum();
        let measured: u64 = test.receipts.iter().map(|r| r.bytes_measured()).sum();
        let retransmits: u64 = test.receipts.iter().map(|r| r.retransmits as u64).sum();
        let steady = client_measured_duration(&test.receipts);
        let raw_mbits =
            (total as f64 * 8.0) / 1_000_000.0 / duration.as_secs_f64().max(0.000_001);

        if retransmits > 0 {
            println!("[CLIENT] TCP retransmits observed: {}", retransmits);
        }

        if test.config.omit > 0 && steady.as_secs_f64() > 0.0 {
            let steady_mbits =
                (measured as f64 * 8.0) / 1_000_000.0 / steady.as_secs_f64().max(0.000_001);
            println!(
                "[CLIENT] sent {} bytes ({} post-omit) across {} stream(s); raw {:.6}s ({:.2} Mbits/sec), steady-state {:.6}s ({:.2} Mbits/sec)",
                total,
                measured,
                test.receipts.len(),
                duration.as_secs_f64(),
                raw_mbits,
                steady.as_secs_f64(),
                steady_mbits,
            );
        } else {
            println!(
                "[CLIENT] sent {} bytes across {} stream(s) in {:.6}s ({:.2} Mbits/sec)",
                total,
                test.receipts.len(),
                duration.as_secs_f64(),
                raw_mbits,
            );
        }
    }

    let Some(ref mut protocol) = test.control_channel else {
        return;
    };

    if let Err(e) = send_framed_json(protocol.transfer.as_mut(), &client_results) {
        eprintln!("failed to send results: {:?}", e);
        return;
    }
    println!("Results sent.");

    if let Err(e) = protocol.transfer.set_nonblocking(false) {
        eprintln!("failed to set blocking for results read: {:?}", e);
        return;
    }

    match recv_framed_json::<Results>(protocol.transfer.as_mut()) {
        Ok(parsed) => println!("server results: {:?}", parsed),
        Err(e) => eprintln!("could not receive/parse server results: {:?}", e),
    }

    if let Err(e) = protocol.transfer.set_nonblocking(true) {
        eprintln!("failed to restore non-blocking after results read: {:?}", e);
    }
}

/// Build the client-side `Results` payload from the receipts each
/// stream thread returned. Per-stream bytes come straight from the
/// send loop's accumulator — nothing derived from the wall clock here,
/// so a slow or fast TCP stack gets attributed fairly.
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
            jitter: 0.0,
            errors: 0,
            packets: 0,
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

/// Measured test duration on the client side: from the earliest
/// first-send across any stream to the latest last-send across any
/// stream. `None` if no stream saw any bytes.
pub fn client_session_duration(
    receipts: &[crate::common::test::ClientStreamReceipt],
) -> Option<std::time::Duration> {
    let first = receipts.iter().filter_map(|r| r.first_send_at).min()?;
    let last = receipts.iter().filter_map(|r| r.last_send_at).max()?;
    if last >= first {
        Some(last - first)
    } else {
        None
    }
}

/// Steady-state duration on the client side: from the earliest
/// first-measured-send (post-omit) to the latest last-send. Zero when
/// no post-omit bytes were observed.
pub fn client_measured_duration(
    receipts: &[crate::common::test::ClientStreamReceipt],
) -> std::time::Duration {
    let first = receipts.iter().filter_map(|r| r.first_measured_at).min();
    let last = receipts.iter().filter_map(|r| r.last_send_at).max();
    match (first, last) {
        (Some(f), Some(l)) if l >= f => l - f,
        _ => std::time::Duration::ZERO,
    }
}

fn create_streams(test: &Test) {
    let host = test.config.host_port();
    let n = test.config.parallel.max(1);
    for stream_id in 1..=n {
        Stream::start(test, host.clone(), test.cookie, stream_id);
    }
}

fn send_iperf_done(test: &mut Test) {
    if let Some(ref mut protocol) = test.control_channel {
        log_send(
            send_control_byte(protocol.transfer.as_mut(), Message::IperfDone),
            "iPerf done sent.",
            "IperfDone",
        );
    }
}

fn send_options(test: &mut Test) {
    let Some(ref mut protocol) = test.control_channel else {
        return;
    };

    let mut options =
        ClientOptions::tcp_defaults(test.config.time, test.config.parallel, test.config.len);
    options.omit = test.config.omit;
    match send_framed_json(protocol.transfer.as_mut(), &options) {
        Ok(()) => println!("Options sent."),
        Err(e) => eprintln!("failed to send options: {:?}", e),
    }
}

fn log_send(result: std::io::Result<()>, success: &str, what: &str) {
    match result {
        Ok(()) => println!("{}", success),
        Err(e) => eprintln!("failed to send {}: {:?}", what, e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::test::ClientStreamReceipt;
    use std::time::{Duration, Instant};

    fn client_receipt_with(
        stream_id: u32,
        bytes: u64,
        first: Option<Instant>,
        last: Option<Instant>,
    ) -> ClientStreamReceipt {
        ClientStreamReceipt {
            stream_id,
            bytes_sent: bytes,
            bytes_omit: 0,
            first_send_at: first,
            first_measured_at: first,
            last_send_at: last,
            retransmits: 0,
            jitter_ms: 0.0,
            lost: 0,
            ooo: 0,
            packets: 0,
        }
    }

    #[test]
    fn build_client_results_populates_one_stream_per_receipt() {
        let receipts = [
            client_receipt_with(1, 1_024, None, None),
            client_receipt_with(2, 4_096, None, None),
        ];

        let r = build_client_results(&receipts, &CpuUsage::ZERO);
        assert_eq!(r.streams.len(), 2);
        assert_eq!(r.streams[0].id, 1);
        assert_eq!(r.streams[0].bytes, 1_024);
        assert_eq!(r.streams[1].id, 2);
        assert_eq!(r.streams[1].bytes, 4_096);
    }

    #[test]
    fn build_client_results_is_empty_on_no_receipts() {
        let r = build_client_results(&[], &CpuUsage::ZERO);
        assert!(r.streams.is_empty());
    }

    #[test]
    fn client_session_duration_spans_earliest_to_latest() {
        let base = Instant::now();
        let receipts = [
            client_receipt_with(1, 10, Some(base), Some(base + Duration::from_millis(500))),
            client_receipt_with(
                2,
                20,
                Some(base + Duration::from_millis(100)),
                Some(base + Duration::from_millis(900)),
            ),
        ];
        assert_eq!(
            client_session_duration(&receipts),
            Some(Duration::from_millis(900))
        );
    }

    #[test]
    fn client_session_duration_is_none_when_no_bytes_sent() {
        let receipts = [
            ClientStreamReceipt::empty(1),
            ClientStreamReceipt::empty(2),
        ];
        assert_eq!(client_session_duration(&receipts), None);
    }

    #[test]
    fn client_measured_duration_uses_first_measured_at() {
        let base = Instant::now();
        // No post-omit bytes: returns zero.
        let unmeasured = ClientStreamReceipt::empty(1);
        assert_eq!(
            client_measured_duration(&[unmeasured]),
            Duration::ZERO
        );

        // With first_measured_at set, spans first_measured_at -> last.
        let mut r = ClientStreamReceipt::empty(2);
        r.bytes_sent = 100;
        r.first_send_at = Some(base);
        r.first_measured_at = Some(base + Duration::from_millis(200));
        r.last_send_at = Some(base + Duration::from_millis(800));
        assert_eq!(
            client_measured_duration(&[r]),
            Duration::from_millis(600)
        );
    }
}
