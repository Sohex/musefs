//! CLI-only graceful unmount on stop signals. Installed by `run_mount`; never
//! in the `musefs-fuse` library (which must not hijack process signals).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

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

/// Try each unmount command until one succeeds. Best-effort: a stop signal must
/// never panic the process.
fn run_unmount(mountpoint: &Path) {
    for (prog, args) in unmount_commands(mountpoint) {
        if let Ok(status) = std::process::Command::new(prog).args(&args).status()
            && status.success()
        {
            return;
        }
    }
    eprintln!(
        "musefs: could not unmount {} after stop signal; run `fusermount3 -u {}` manually",
        mountpoint.display(),
        mountpoint.display()
    );
}

/// Spawn a thread that unmounts `mountpoint` on the first SIGTERM/SIGINT, so a
/// `Ctrl-C` / `systemctl stop` / container stop unwinds the blocking mount
/// cleanly instead of leaving a stale FUSE endpoint. CLI-only.
pub fn install_unmount_on_signal(mountpoint: PathBuf) -> std::io::Result<()> {
    use signal_hook::consts::{SIGINT, SIGTERM};
    use signal_hook::iterator::Signals;

    let mut signals = Signals::new([SIGINT, SIGTERM])?;
    std::thread::Builder::new()
        .name("musefs-unmount-on-signal".into())
        .spawn(move || {
            if signals.forever().next().is_some() {
                run_unmount(&mountpoint);
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
