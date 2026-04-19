use clap::Parser;

use rperf3::cli::{Cli, Mode};

fn main() {
    let parsed = Cli::parse();

    match parsed.into_mode() {
        Mode::Client(config) => rperf3::run_client(config),
        Mode::Server(config) => rperf3::run_server(config),
    }
}
