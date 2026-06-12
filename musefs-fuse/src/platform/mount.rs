//! Per-OS FUSE mount options. The common set (read-only, filesystem name) is
//! shared; macOS adds a volume name and suppresses AppleDouble sidecar noise.

use fuser::MountOption;

/// Read-only mount options for `fs_name`, plus any per-OS additions. With
/// `allow_other`, also mount `allow_other` + `default_permissions` so an account
/// other than the mounting user can reach the mount and the presented owner/mode
/// bits are kernel-enforced.
pub fn options(fs_name: &str, allow_other: bool) -> Vec<MountOption> {
    let mut opts = vec![MountOption::RO, MountOption::FSName(fs_name.to_string())];
    if allow_other {
        opts.push(MountOption::CUSTOM("allow_other".to_string()));
        opts.push(MountOption::DefaultPermissions);
    }
    extend_os_specific(&mut opts, fs_name);
    opts
}

/// Mount-time guard for `allow_other`: libfuse refuses an `allow_other` mount for
/// a non-root user unless `/etc/fuse.conf` enables `user_allow_other`. Check it
/// up front to replace fusermount3's cryptic "Permission denied" with actionable
/// guidance. Non-Linux platforms don't gate on `/etc/fuse.conf`, so it's a no-op.
pub fn check_allow_other(allow_other: bool) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let is_root = rustix::process::geteuid().as_raw() == 0;
        // `.ok()` collapses any read failure (missing, EACCES, dangling symlink)
        // to `None`, which the decision treats as "not permitted" — fail-safe.
        let conf = std::fs::read_to_string("/etc/fuse.conf").ok();
        preflight_decision(allow_other, is_root, conf.as_deref())
            .map_err(|msg| std::io::Error::new(std::io::ErrorKind::PermissionDenied, msg))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = allow_other;
        Ok(())
    }
}

#[cfg(target_os = "linux")]
const ALLOW_OTHER_HELP: &str = "allow_other is enabled (via --allow-other, or implied by --owner/--group) \
but '/etc/fuse.conf' does not enable 'user_allow_other'; libfuse refuses a non-root allow_other mount without it. \
Add a line 'user_allow_other' to /etc/fuse.conf, or run musefs as root.";

/// True if `contents` has an active `user_allow_other` directive: a line whose
/// text before any `#` comment trims to exactly `user_allow_other`.
#[cfg(target_os = "linux")]
fn user_allow_other_active(contents: &str) -> bool {
    contents
        .lines()
        .any(|line| line.split('#').next().unwrap_or("").trim() == "user_allow_other")
}

/// Pure pre-flight decision. `conf` is `None` when `/etc/fuse.conf` could not be
/// read; treated as "not permitted" so the actionable error fires (a false
/// positive is harmless — fusermount3 would have failed the mount anyway). Root
/// and the no-`allow_other` case always pass.
#[cfg(target_os = "linux")]
fn preflight_decision(allow_other: bool, is_root: bool, conf: Option<&str>) -> Result<(), String> {
    if !allow_other || is_root {
        return Ok(());
    }
    if conf.is_some_and(user_allow_other_active) {
        return Ok(());
    }
    Err(ALLOW_OTHER_HELP.to_string())
}

#[cfg(target_os = "macos")]
fn extend_os_specific(opts: &mut Vec<MountOption>, fs_name: &str) {
    // fuser 0.17 has no `VolName` variant; macOS-specific options go through
    // CUSTOM. `noappledouble` stops Finder writing ._ sidecar files.
    opts.push(MountOption::CUSTOM(format!("volname={fs_name}")));
    opts.push(MountOption::CUSTOM("noappledouble".to_string()));
}

#[cfg(not(target_os = "macos"))]
fn extend_os_specific(_opts: &mut Vec<MountOption>, _fs_name: &str) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_are_always_read_only_and_named() {
        let opts = options("musefs", false);
        assert!(opts.contains(&MountOption::RO));
        assert!(opts.contains(&MountOption::FSName("musefs".to_string())));
    }

    #[test]
    fn allow_other_adds_allow_other_and_default_permissions() {
        let opts = options("musefs", true);
        assert!(opts.contains(&MountOption::CUSTOM("allow_other".to_string())));
        assert!(opts.contains(&MountOption::DefaultPermissions));
    }

    #[test]
    fn no_allow_other_omits_allow_other_and_default_permissions() {
        let opts = options("musefs", false);
        assert!(!opts.contains(&MountOption::CUSTOM("allow_other".to_string())));
        assert!(!opts.contains(&MountOption::DefaultPermissions));
    }
}

#[cfg(all(test, target_os = "macos"))]
mod macos_tests {
    use super::*;

    #[test]
    fn macos_adds_volname_and_noappledouble() {
        let opts = options("musefs", false);
        assert!(opts.contains(&MountOption::CUSTOM("volname=musefs".to_string())));
        assert!(opts.contains(&MountOption::CUSTOM("noappledouble".to_string())));
    }
}

#[cfg(all(test, target_os = "linux"))]
mod preflight_tests {
    use super::*;

    #[test]
    fn parser_accepts_active_directive_forms() {
        assert!(user_allow_other_active("user_allow_other"));
        assert!(user_allow_other_active("   user_allow_other   "));
        assert!(user_allow_other_active(
            "user_allow_other # enable for media server"
        ));
        assert!(user_allow_other_active(
            "mount_max=1000\nuser_allow_other\n"
        ));
    }

    #[test]
    fn parser_rejects_inactive_or_absent() {
        assert!(!user_allow_other_active("# user_allow_other"));
        assert!(!user_allow_other_active("#user_allow_other"));
        assert!(!user_allow_other_active("mount_max=1000"));
        assert!(!user_allow_other_active(""));
    }

    #[test]
    fn preflight_passes_when_not_requested_or_root() {
        assert!(preflight_decision(false, false, None).is_ok());
        assert!(preflight_decision(true, true, None).is_ok());
    }

    #[test]
    fn preflight_requires_directive_for_nonroot() {
        assert!(preflight_decision(true, false, Some("user_allow_other")).is_ok());
        assert!(preflight_decision(true, false, Some("# nope")).is_err());
        assert!(preflight_decision(true, false, None).is_err());
    }

    #[test]
    fn preflight_error_is_self_contained() {
        let err = preflight_decision(true, false, None).unwrap_err();
        assert!(err.contains("/etc/fuse.conf"));
        assert!(err.contains("user_allow_other"));
    }
}
