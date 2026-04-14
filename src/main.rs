use clap::Parser;

use rperf::cli::{Cli, Mode};

fn main() {
    let parsed = Cli::parse();

    match parsed.into_mode() {
        Mode::Client(config) => rperf::run_client(config),
        Mode::Server(config) => rperf::run_server(config),
    }

    println!("Finished.");
}
