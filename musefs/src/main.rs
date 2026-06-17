use clap::Parser;
use musefs_cli::{Cli, run};

#[cfg(feature = "jemalloc")]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

/// Enable jemalloc's background purge thread so an idle daemon returns dirty
/// pages to the OS (the RSS-creep fix in #360). Best-effort: unsupported on some
/// platforms (notably macOS), where it logs at debug and continues — jemalloc
/// stays active and still purges on allocation activity.
#[cfg(feature = "jemalloc")]
fn enable_jemalloc_background_thread() {
    if let Err(e) = tikv_jemalloc_ctl::background_thread::write(true) {
        log::debug!("jemalloc background_thread unavailable: {e}");
    }
}

/// Allocator-stats probe for the `.musefs-metrics` surface (#394). Lives here
/// because reading jemalloc stats requires linking `tikv-jemalloc-ctl`, and only
/// this binary installs jemalloc as the `#[global_allocator]`; `musefs-fuse`
/// stays allocator-agnostic and receives this via `set_alloc_probe`. Best-effort:
/// any ctl failure maps to `None` rather than panicking.
#[cfg(feature = "jemalloc")]
fn jemalloc_stats() -> Option<musefs_fuse::AllocatorStats> {
    use tikv_jemalloc_ctl::{epoch, stats};
    epoch::advance().ok()?;
    Some(musefs_fuse::AllocatorStats {
        allocated: stats::allocated::read().ok()? as u64,
        resident: stats::resident::read().ok()? as u64,
        active: stats::active::read().ok()? as u64,
        retained: stats::retained::read().ok()? as u64,
    })
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    // The library crates report serve-path failures through the `log` facade;
    // without a sink they vanish. Default to `warn` so they surface on stderr;
    // `-v`/`-vv`/`-vvv` raise the floor, and an explicit RUST_LOG overrides both.
    let default_level = match cli.verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default_level))
        .init();
    #[cfg(feature = "jemalloc")]
    {
        enable_jemalloc_background_thread();
        musefs_fuse::set_alloc_probe(jemalloc_stats);
    }
    match run(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("musefs: {e:#}");
            std::process::ExitCode::from(1)
        }
    }
}

#[cfg(all(test, feature = "jemalloc"))]
mod tests {
    #[cfg(target_os = "linux")]
    #[test]
    fn background_thread_enables_on_linux() {
        super::enable_jemalloc_background_thread();
        assert!(
            tikv_jemalloc_ctl::background_thread::read().unwrap(),
            "background_thread should be on after enable() on linux"
        );
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn enable_background_thread_does_not_panic_off_linux() {
        // jemalloc lacks background-thread support on some platforms (macOS);
        // the helper must swallow the error rather than panic.
        super::enable_jemalloc_background_thread();
    }

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
