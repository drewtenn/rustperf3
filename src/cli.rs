//! CLI parsing for `rperf`.
//!
//! Accepts either `-s` (server mode) or `-c <host>` (client mode) as
//! mutually-exclusive top-level flags — one must be given. Parsed
//! [`Cli`] values convert into a [`Mode`] that the binary dispatches
//! on, keeping the rest of the code free of clap-specific types.

use clap::{ArgGroup, Parser};

use crate::common::test::Config;
use crate::common::wire::DEFAULT_TCP_LEN;

/// Default iPerf3 control port.
pub const DEFAULT_PORT: u16 = 5201;
/// Default test duration in seconds, matching iPerf3.
pub const DEFAULT_TIME_SECS: u32 = 10;
/// Default parallel stream count.
pub const DEFAULT_PARALLEL: u32 = 1;
/// Default listen address when running as a server.
pub const DEFAULT_BIND: &str = "0.0.0.0";
/// Default omit-window length in seconds. Matches iPerf3's behavior of
/// not omitting anything unless explicitly requested.
pub const DEFAULT_OMIT_SECS: u32 = 0;

#[derive(Parser, Debug, Clone, PartialEq, Eq)]
#[command(name = "rperf", version, about = "Rust iPerf3-compatible client/server")]
#[command(group(
    ArgGroup::new("mode")
        .required(true)
        .args(["server", "host"]),
))]
pub struct Cli {
    /// Run as a server (listen for incoming connections).
    #[arg(short = 's', long = "server")]
    pub server: bool,

    /// Run as a client and connect to this server host.
    #[arg(short = 'c', long = "client", value_name = "HOST")]
    pub host: Option<String>,

    /// Server port.
    #[arg(short = 'p', long = "port", default_value_t = DEFAULT_PORT)]
    pub port: u16,

    /// Test duration in seconds (client-side).
    #[arg(short = 't', long = "time", default_value_t = DEFAULT_TIME_SECS)]
    pub time: u32,

    /// Number of parallel streams (client-side).
    #[arg(short = 'P', long = "parallel", default_value_t = DEFAULT_PARALLEL)]
    pub parallel: u32,

    /// Bytes per write (TCP buffer length).
    #[arg(short = 'l', long = "len", default_value_t = DEFAULT_TCP_LEN as u32)]
    pub len: u32,

    /// Seconds to omit at the start of the test (excludes TCP slow
    /// start from the reported measurement window). iPerf3-compatible.
    #[arg(short = 'O', long = "omit", default_value_t = DEFAULT_OMIT_SECS)]
    pub omit: u32,
}

/// Result of a successful parse: either client or server mode, each
/// carrying a fully-populated `Config`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Client(Config),
    Server(Config),
}

impl Cli {
    pub fn into_mode(self) -> Mode {
        let base = Config {
            host: self.host.clone().unwrap_or_else(|| DEFAULT_BIND.to_string()),
            port: self.port,
            time: self.time,
            parallel: self.parallel,
            len: self.len,
            omit: self.omit,
        };
        if self.server {
            Mode::Server(base)
        } else {
            Mode::Client(base)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parses_full_client_flag_set() {
        let cli = Cli::try_parse_from([
            "rperf", "-c", "127.0.0.1", "-p", "5202", "-t", "3", "-P", "2", "-l", "65536",
        ])
        .expect("parse");

        assert!(!cli.server);
        assert_eq!(cli.host.as_deref(), Some("127.0.0.1"));
        assert_eq!(cli.port, 5202);
        assert_eq!(cli.time, 3);
        assert_eq!(cli.parallel, 2);
        assert_eq!(cli.len, 65_536);
    }

    #[test]
    fn applies_defaults_when_only_host_given() {
        let cli = Cli::try_parse_from(["rperf", "-c", "example.com"]).expect("parse");
        assert_eq!(cli.port, DEFAULT_PORT);
        assert_eq!(cli.time, DEFAULT_TIME_SECS);
        assert_eq!(cli.parallel, DEFAULT_PARALLEL);
        assert_eq!(cli.len, DEFAULT_TCP_LEN as u32);
    }

    #[test]
    fn parses_server_only_mode() {
        let cli = Cli::try_parse_from(["rperf", "-s"]).expect("parse");
        assert!(cli.server);
        assert_eq!(cli.host, None);
        assert_eq!(cli.port, DEFAULT_PORT);
    }

    #[test]
    fn parses_server_with_port() {
        let cli = Cli::try_parse_from(["rperf", "-s", "-p", "5202"]).expect("parse");
        assert!(cli.server);
        assert_eq!(cli.port, 5202);
    }

    #[test]
    fn server_and_client_conflict() {
        let err = Cli::try_parse_from(["rperf", "-s", "-c", "1.2.3.4"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn missing_both_server_and_client_is_error() {
        assert!(Cli::try_parse_from(["rperf"]).is_err());
    }

    #[test]
    fn client_mode_uses_host() {
        let cli = Cli::try_parse_from(["rperf", "-c", "10.0.0.1", "-p", "5202"]).unwrap();
        match cli.into_mode() {
            Mode::Client(cfg) => {
                assert_eq!(cfg.host, "10.0.0.1");
                assert_eq!(cfg.port, 5202);
                assert_eq!(cfg.host_port(), "10.0.0.1:5202");
            }
            Mode::Server(_) => panic!("expected client mode"),
        }
    }

    #[test]
    fn server_mode_uses_default_bind() {
        let cli = Cli::try_parse_from(["rperf", "-s", "-p", "5202"]).unwrap();
        match cli.into_mode() {
            Mode::Server(cfg) => {
                assert_eq!(cfg.host, DEFAULT_BIND);
                assert_eq!(cfg.port, 5202);
            }
            Mode::Client(_) => panic!("expected server mode"),
        }
    }
}
