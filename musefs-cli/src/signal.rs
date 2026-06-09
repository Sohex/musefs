//! CLI-only graceful unmount on stop signals. Installed by `run_mount`; never
//! in the `musefs-fuse` library (which must not hijack process signals).
//!
//! The handler shells out to `fusermount3 -u` from a dedicated thread and never
//! touches core state — the slab, DB connections, or session locks. That is the
//! durable reason for the external-command design (beyond fuser 0.17 not
//! exposing a usable in-process unmounter): it cannot deadlock against an
//! in-flight read or a `poll_refresh` worker that holds those guards, because
//! the kernel drives the unmount, not us.
//!
//! Exit is *bounded*. A plain `fusermount3 -u` fails `EBUSY` (or, for a wedged
//! backing store — dead NFS, a spun-down disk — never lets the in-flight read
//! drain) when the mount is busy, so the blocking mount would not return. If the
//! unmount has not let it return within [`GRACE`], the handler escalates to a
//! lazy detach and hard-exits, so a stuck backing store cannot make us outlast
//! the init system's stop-timeout escalation (systemd's default
//! `TimeoutStopSec` is 90s).

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Window allowed for a clean unmount to let the blocking mount return before we
/// escalate to a lazy detach + forced exit. Comfortably under systemd's default
/// `TimeoutStopSec` (90s) so we exit well before it would SIGKILL us.
const GRACE: Duration = Duration::from_secs(5);

/// Unmount commands tried in order, most-preferred first, as `(program, args)`.
/// `fusermount3`/`fusermount` are the unprivileged FUSE unmount tools; `umount`
/// is the last resort.
fn unmount_commands(mountpoint: &Path) -> Vec<(&'static str, Vec<OsString>)> {
    let mp = mountpoint.as_os_str().to_owned();
    vec![
        ("fusermount3", vec!["-u".into(), mp.clone()]),
        ("fusermount", vec!["-u".into(), mp.clone()]),
        ("umount", vec![mp]),
    ]
}

/// Try each unmount command until one succeeds. Errors are logged and swallowed:
/// the goal is a bounded exit, not unmount success — the mount may already be
/// gone (a manual `fusermount3 -u`, or an `ENOTCONN` dead endpoint), in which
/// case every command "fails" and that is fine.
fn run_unmount(mountpoint: &Path) {
    for (prog, args) in unmount_commands(mountpoint) {
        if let Ok(status) = std::process::Command::new(prog).args(&args).status()
            && status.success()
        {
            return;
        }
    }
    eprintln!(
        "musefs: could not unmount {} after stop signal; detaching lazily and exiting",
        mountpoint.display()
    );
}

/// Best-effort lazy detach (`fusermount3 -u -z`, i.e. `MNT_DETACH`): drops the
/// mountpoint from the namespace even while it is busy. The escalation step
/// before a forced exit.
fn lazy_detach(mountpoint: &Path) {
    let _ = std::process::Command::new("fusermount3")
        .args([OsStr::new("-u"), OsStr::new("-z"), mountpoint.as_os_str()])
        .status();
}

/// First stop signal: attempt a clean unmount, then guarantee a bounded exit.
/// On the happy path the unmount EOFs `/dev/fuse`, the blocking mount returns,
/// and `main` exits on its own before [`GRACE`] elapses (this thread is torn
/// down with the process). If the mount is busy/wedged, we escalate and
/// hard-exit instead of waiting on a join that may never come.
fn graceful_unmount_then_exit(mountpoint: &Path) -> ! {
    run_unmount(mountpoint);
    std::thread::sleep(GRACE);
    lazy_detach(mountpoint);
    std::process::exit(0);
}

/// Spawn a thread that, on the first SIGTERM/SIGINT, unmounts `mountpoint` with
/// a bounded graceful sequence (so `Ctrl-C` / `systemctl stop` / container stop
/// unwinds the blocking mount cleanly), and hard-exits on a second signal — the
/// operator/init system wants out now. CLI-only.
pub fn install_unmount_on_signal(mountpoint: PathBuf) -> std::io::Result<()> {
    use signal_hook::consts::{SIGINT, SIGTERM};
    use signal_hook::iterator::Signals;

    let mut signals = Signals::new([SIGINT, SIGTERM])?;
    std::thread::Builder::new()
        .name("musefs-unmount-on-signal".into())
        .spawn(move || {
            let mut graceful_started = false;
            for _signal in signals.forever() {
                if graceful_started {
                    // Second stop signal while the first unmount is still in
                    // flight: don't wait on anything, leave now.
                    std::process::exit(130);
                }
                graceful_started = true;
                // Drive the bounded unmount on its own thread so this loop stays
                // responsive to that second signal. If the thread can't be
                // spawned (resource exhaustion), unmount inline rather than
                // swallow the signal and leave the mount up.
                let mp = mountpoint.clone();
                if let Err(e) = std::thread::Builder::new()
                    .name("musefs-unmount".into())
                    .spawn(move || graceful_unmount_then_exit(&mp))
                {
                    eprintln!("musefs: could not spawn unmount thread ({e}); unmounting inline");
                    graceful_unmount_then_exit(&mountpoint);
                }
            }
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::path::Path;

    #[test]
    fn unmount_commands_try_fusermount3_then_fallbacks() {
        let cmds = unmount_commands(Path::new("/mnt/x"));
        let progs: Vec<&str> = cmds.iter().map(|(p, _)| *p).collect();
        assert_eq!(progs, ["fusermount3", "fusermount", "umount"]);
        // fusermount variants pass `-u <mp>`; umount passes just `<mp>`.
        assert_eq!(
            cmds[0].1,
            vec![OsString::from("-u"), OsString::from("/mnt/x")]
        );
        assert_eq!(cmds[2].1, vec![OsString::from("/mnt/x")]);
    }
}
