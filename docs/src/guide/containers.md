# Running in containers

### Container images

Each tagged release also publishes multi-arch images to the GitHub Container
Registry:

| Image | libc | Platforms |
| --- | --- | --- |
| `ghcr.io/sohex/musefs:<version>`, `ghcr.io/sohex/musefs:latest` | glibc | amd64, arm64, riscv64 |
| `ghcr.io/sohex/musefs:<version>-musl`, `ghcr.io/sohex/musefs:musl` | musl | amd64, arm64, riscv64 |

`docker pull` selects the CPU architecture automatically. Use the `-musl` /
`:musl` tags when slotting musefs into an Alpine-based stack; the default
(glibc) tags suit everything else. Floating `:latest` / `:musl` track the most
recent stable release only — prereleases publish only version-pinned tags.

**Running musefs on the host is the simplest, best-supported option** — it is an
ordinary FUSE daemon and the image exists mainly to colocate musefs with
containerized media managers (e.g. Lidarr). If you do containerize, mind the
gotchas below.

#### Required flags

musefs mounts via FUSE, so the container needs `/dev/fuse` and the matching
capability:

```bash
docker run --rm \
  --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor=unconfined \
  -v /path/to/library:/library:ro \
  -v /path/to/store:/store \
  ghcr.io/sohex/musefs:latest scan /library --db /store/musefs.db
```

Without `--device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor=unconfined`
the mount cannot be established.

> **Note:** The apparmor flag may or may not be necessary depending on how your system is configured.

Note that `CAP_SYS_ADMIN` is a broadly privileged capability — it grants far more
than FUSE mounting (mounting arbitrary filesystems, and more). It is unavoidable
for an in-container FUSE mount — even rootless Podman cannot drop it; without
`--cap-add SYS_ADMIN` the mount fails with `fusermount3: mount failed: Permission
denied`. Under rootless Podman the capability is confined to the container's user
namespace rather than the host, so its blast radius is smaller, but it is still
required. Running musefs on the host needs no such capability at all.

#### Runs as a non-root user

The images run as a dedicated unprivileged user (default uid/gid 1000), not
root — musefs mounts via the setuid `fusermount3` helper and needs no root of
its own. Consequences for the commands above:

- The bind-mounted **store** volume must be writable by that uid. Either
  `chown 1000:1000 /path/to/store` on the host, or add `--user $(id -u):$(id -g)`
  to run as your own uid. The **library** volume is mounted `:ro`, so its
  ownership does not matter.
- To bake an image whose user matches your host account (so no `chown` or
  `--user` is needed), build from source with
  `--build-arg MUSEFS_UID=$(id -u) --build-arg MUSEFS_GID=$(id -g)`.
- The images include `user_allow_other` in `/etc/fuse.conf`, so a non-root
  `--allow-other` / `--owner` / `--group` mount (needed to share the mount across
  containers or users, below) passes musefs's pre-flight check. See
  [Ownership and permissions](configuration.md#ownership-and-permissions).

#### The mount-visibility gotcha (read this before sharing the mount)

A FUSE mount made inside a container lives in that container's mount namespace.
By default neither the host nor other containers can see it, so pointing a second
container (your media manager) at musefs's output does not work out of the box.
To share the mount you propagate it between containers through a host directory:
musefs binds that directory with `rshared` and mounts itself there, and the
consumer binds the same directory with `rslave` so the mount propagates in. The
host directory must itself be a shared mount.

```bash
# A host directory both containers bind to, marked shared so mounts propagate.
mkdir -p /srv/musefs-mnt
mount --bind /srv/musefs-mnt /srv/musefs-mnt
mount --make-rshared /srv/musefs-mnt

# A named volume for the store, writable by the image's unprivileged user.
podman volume create musefs-store

# musefs container: bind rshared, mount musefs there with --allow-other.
podman run -d --name musefs \
  --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor=unconfined \
  -v /path/to/library:/library:ro -v musefs-store:/store \
  --mount type=bind,source=/srv/musefs-mnt,destination=/mnt/musefs,bind-propagation=rshared \
  ghcr.io/sohex/musefs:latest mount /mnt/musefs --db /store/musefs.db --allow-other

# consumer container: bind the same host path rslave; the mount propagates in.
podman run -d --name player \
  --mount type=bind,source=/srv/musefs-mnt,destination=/music,bind-propagation=rslave \
  ghcr.io/sohex/yourmediamanager:latest
```

Use a named volume (or an already-writable host path) for the store: a bind from
a root-owned host directory is read-only to the image's unprivileged user and
musefs aborts before mounting. `--allow-other` is required because the consumer
container runs as a different uid than the musefs container; without it the
consumer gets `Permission denied` on the mount. See
[Ownership and permissions](configuration.md#ownership-and-permissions).

> **Note:** Some hardened kernels block cross-uid access to an unprivileged user's FUSE
> mount even with `--allow-other` — for example when the fuse module's
> `allow_sys_admin_access` parameter is `N`, or unprivileged user namespaces are
> restricted. If the consumer still gets `Permission denied`, set
> `/sys/module/fuse/parameters/allow_sys_admin_access` to `Y`, or run musefs and
> the consumer under the same uid.

Both the glibc and musl images carry the `fuse3` userspace tools; pick `:musl`
if your other containers are Alpine-based, otherwise the default tags are fine.

#### Sharing a host mount into a container

Running musefs on the host instead of in a container is simpler and needs no
`CAP_SYS_ADMIN`. Mark the mount point as shared and mount musefs there with
`--allow-other`, then bind it into the consumer container with `rslave` so the
host's musefs mount propagates in:

```bash
# On the host: mark the mount point shared, then mount musefs with --allow-other.
mkdir -p /srv/musefs-mnt
mount --bind /srv/musefs-mnt /srv/musefs-mnt
mount --make-rshared /srv/musefs-mnt
musefs mount /srv/musefs-mnt --db /store/musefs.db --allow-other &

podman run -d \
  --mount type=bind,source=/srv/musefs-mnt,destination=/music,bind-propagation=rslave \
  ghcr.io/sohex/yourmediamanager:latest
# the container reads the re-tagged view at /music, byte-for-byte live
```

`rslave` is what keeps this working across restarts: a plain bind only captures
whatever is mounted when the container starts, so it shows an empty directory if
musefs mounts later and a stale view after a musefs restart.
