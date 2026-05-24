use clap::Parser;
use musefs_cli::{run, Cli};

fn main() {
    if let Err(e) = run(Cli::parse()) {
        eprintln!("musefs: {e:#}");
        std::process::exit(1);
    }
}
