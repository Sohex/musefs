//! Kernel FUSE passthrough is Linux-only (Linux 6.9+). On Linux this registers
//! the backing fd with the kernel so reads bypass the daemon; on every other OS
//! the path is a no-op and reads are served through the daemon.
//!
//! Each `imp` module carries its own `use` lines; the public surface is the
//! `pub use` re-export at the bottom, so there are no top-level imports here.

#[cfg(target_os = "linux")]
mod imp {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, PoisonError};

    use fuser::{BackingId, FileHandle, FopenFlags, InitFlags, KernelConfig, ReplyOpen};
    use musefs_core::{Fh, Musefs};

    /// Live passthrough state: kernel-registered backing fds keyed by wire fh,
    /// plus a sticky disable flag flipped on the first `open_backing` failure
    /// (kernel < 6.9 / ioctl unsupported) so later opens skip the doomed ioctl.
    #[derive(Clone)]
    pub struct PassthroughState {
        backing: Arc<Mutex<HashMap<u64, BackingId>>>,
        disabled: Arc<AtomicBool>,
    }

    impl PassthroughState {
        pub fn new(structure_only: bool) -> PassthroughState {
            let disabled = structure_only && definitely_lacks_cap_sys_admin();
            if disabled {
                log::info!(
                    "StructureOnly mount without CAP_SYS_ADMIN: kernel passthrough unavailable; reads will be served by the daemon"
                );
            }
            PassthroughState {
                backing: Arc::new(Mutex::new(HashMap::new())),
                disabled: Arc::new(AtomicBool::new(disabled)),
            }
        }

        /// Drop the backing registration for `fh` (fires the backing-close ioctl
        /// via `BackingId`'s Drop). A no-op for plain handles not in the map.
        pub fn remove(&self, fh: u64) {
            self.backing
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&fh);
        }
    }

    /// Reply to `open`: try kernel passthrough, else serve through the daemon.
    pub fn reply_open(
        pt: &PassthroughState,
        core: &Musefs,
        fh: Fh,
        reply: ReplyOpen,
        plain_flags: FopenFlags,
    ) {
        if !pt.disabled.load(Ordering::Relaxed) {
            if let Some(pfd) = core.passthrough_fd(fh) {
                match reply.open_backing(&pfd) {
                    Ok(id) => {
                        // Insert before the reply: the kernel cannot release an
                        // fh it has not yet seen. FOPEN_KEEP_CACHE is dropped -
                        // page-cache ownership belongs to the backing inode here.
                        let mut map = pt.backing.lock().unwrap_or_else(PoisonError::into_inner);
                        let id = map.entry(fh.get()).insert_entry(id).into_mut();
                        return reply.opened_passthrough(
                            FileHandle(fh.get()),
                            FopenFlags::empty(),
                            id,
                        );
                    }
                    Err(e) => {
                        pt.disabled.store(true, Ordering::Relaxed);
                        log::info!(
                            "FUSE passthrough unavailable; serving reads through the daemon: {e}"
                        );
                    }
                }
            }
        }
        reply.opened(FileHandle(fh.get()), plain_flags);
    }

    /// Request the passthrough capability + stack depth during `init`.
    pub fn request_capabilities(config: &mut KernelConfig) {
        // Both calls are required: fuser only copies max_stack_depth into the
        // init reply when FUSE_PASSTHROUGH negotiated; depth 0 disables it.
        // Depth 2 (kernel max) lets backing files live on a stacked fs.
        let _ = config.add_capabilities(InitFlags::FUSE_PASSTHROUGH);
        let _ = config.set_max_stack_depth(2);
    }

    /// True only when /proc/self/status definitively shows CAP_SYS_ADMIN absent.
    /// Unreadable/unparseable -> false (stay neutral; the first open decides).
    fn definitely_lacks_cap_sys_admin() -> bool {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| cap_eff_has_sys_admin(&s))
            .is_some_and(|has| !has)
    }

    /// Parse the `CapEff:` line; None when absent or malformed.
    fn cap_eff_has_sys_admin(status: &str) -> Option<bool> {
        const CAP_SYS_ADMIN_BIT: u32 = 21;
        let hex = status
            .lines()
            .find_map(|l| l.strip_prefix("CapEff:"))?
            .trim();
        let mask = u64::from_str_radix(hex, 16).ok()?;
        Some(mask & (1 << CAP_SYS_ADMIN_BIT) != 0)
    }

    #[cfg(test)]
    mod tests {
        use super::cap_eff_has_sys_admin;

        #[test]
        fn cap_eff_parser_root_mask_has_sys_admin() {
            assert_eq!(
                cap_eff_has_sys_admin("CapPrm:\t0000003fffffffff\nCapEff:\t0000003fffffffff\n"),
                Some(true)
            );
        }

        #[test]
        fn cap_eff_parser_zero_mask_lacks_sys_admin() {
            assert_eq!(
                cap_eff_has_sys_admin("CapEff:\t0000000000000000\n"),
                Some(false)
            );
        }

        #[test]
        fn cap_eff_parser_missing_line_returns_none() {
            assert_eq!(cap_eff_has_sys_admin("Name:\tfoo\nUid:\t1000\n"), None);
        }

        #[test]
        fn cap_eff_parser_garbage_hex_returns_none() {
            assert_eq!(cap_eff_has_sys_admin("CapEff:\tnothex\n"), None);
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use fuser::{FileHandle, FopenFlags, KernelConfig, ReplyOpen};
    use musefs_core::{Fh, Musefs};

    /// Off Linux there is no kernel passthrough; this carries no state.
    #[derive(Clone)]
    pub struct PassthroughState;

    impl PassthroughState {
        pub fn new(structure_only: bool) -> PassthroughState {
            if structure_only {
                log::info!(
                    "StructureOnly mount: kernel passthrough is Linux-only; reads will be served by the daemon"
                );
            }
            PassthroughState
        }

        pub fn remove(&self, _fh: u64) {}
    }

    /// Always serve through the daemon - no passthrough on this OS.
    pub fn reply_open(
        _pt: &PassthroughState,
        _core: &Musefs,
        fh: Fh,
        reply: ReplyOpen,
        plain_flags: FopenFlags,
    ) {
        reply.opened(FileHandle(fh.get()), plain_flags);
    }

    /// No passthrough capability to request off Linux.
    pub fn request_capabilities(_config: &mut KernelConfig) {}
}

pub use imp::{reply_open, request_capabilities, PassthroughState};
