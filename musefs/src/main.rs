use clap::Parser;
use musefs_cli::{Cli, run};

fn main() {
    // The library crates report serve-path failures through the `log` facade;
    // without a sink they vanish. Default to `warn` so they surface on stderr,
    // overridable via RUST_LOG.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    if let Err(e) = run(Cli::parse()) {
        eprintln!("musefs: {e:#}");
        std::process::exit(1);
    }
}
