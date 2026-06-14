use clap::Parser;
use musefs_cli::{Cli, run};

#[cfg(feature = "jemalloc")]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

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

#[cfg(all(test, feature = "jemalloc"))]
mod tests {
    #[test]
    fn jemalloc_is_the_global_allocator() {
        let buf: Vec<u8> = vec![0u8; 4 * 1024 * 1024];
        std::hint::black_box(&buf);
        tikv_jemalloc_ctl::epoch::advance().unwrap();
        let allocated = tikv_jemalloc_ctl::stats::allocated::read().unwrap();
        assert!(
            allocated >= 1 << 20,
            "jemalloc reports {allocated} bytes allocated; not wired as #[global_allocator]"
        );
        drop(buf);
    }
}
