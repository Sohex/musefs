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
