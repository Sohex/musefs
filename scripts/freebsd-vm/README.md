# FreeBSD VM e2e harness

Runs the musefs FUSE end-to-end suite on FreeBSD. CI does this in a VM (the
`freebsd` job in `.github/workflows/ci.yml`) by invoking the two scripts here;
this document is the matching **local** procedure. The scripts are the single
source of truth - CI and local both run them, so they cannot drift.

## What's where

- `provision.sh` - installs the toolchain + `ffmpeg` and loads the `fusefs`
  kernel module.
- `run-e2e.sh` - `cargo test --workspace` then the `--ignored` FUSE e2e suite
  (guards that `ffmpeg` is present so the decode/encode tests don't skip).
- The VM **image** is not committed; keep it under the gitignored `/.scratch/`.

## Local run (qemu example)

1. Put a FreeBSD disk image under `/.scratch/`, e.g.
   `/.scratch/freebsd-14.qcow2` (download an official VM image or build one).
2. Boot it with the repo shared in (9p/virtfs or just `scp`/`git clone` inside):

   ```sh
   qemu-system-x86_64 -m 4096 -smp 4 \
     -drive file=.scratch/freebsd-14.qcow2,if=virtio \
     -nic user,hostfwd=tcp::2222-:22
   ```

3. Get the repo into the VM (clone your branch, or `rsync` the worktree), then
   from the repo root inside the VM, as root:

   ```sh
   sh scripts/freebsd-vm/provision.sh
   sh scripts/freebsd-vm/run-e2e.sh
   ```

`provision.sh` needs root (it runs `pkg install` and `kldload`). If you run the
tests as an unprivileged user, `vfs.usermount=1` (set by `provision.sh`) lets the
mount succeed; otherwise run `run-e2e.sh` as root too.

## Notes

- FreeBSD uses fuser's pure-rust `/dev/fuse` backend - **no libfuse package**;
  only the `fusefs` kernel module and base-system `mount_fusefs(8)` are needed.
- **`ffmpeg` is required** for the full suite: `playback_pcm.rs` (decode-to-PCM
  SHA equality) and `ogg_read_through.rs` (opus/vorbis/flac-in-ogg fixtures)
  shell out to it and skip silently if it is missing - `run-e2e.sh` guards
  against that. The default FreeBSD `ffmpeg` package has the needed codecs.
- Kernel FUSE passthrough (StructureOnly) is **Linux-only**; on FreeBSD it falls
  back to daemon serving. macOS is best-effort (compile + unit only; no mount
  harness yet).
