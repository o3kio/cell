# Packaging (P7-B: systemd units, sd_notify readiness, hardening)

This directory holds the two service units and the packaging notes.
The package build itself is P7-C (`.deb`/`.rpm`, release CI, install
smoke); nothing here installs anything yet.

## What P7-C will install where

| Path | Contents |
|---|---|
| `/usr/bin/volvisord`, `/usr/bin/volvisor-witnessd` | the two binaries from the one package (ADR-0009: one package, separate units) |
| `/usr/lib/systemd/system/volvisord.service` | from `volvisord.service` here |
| `/usr/lib/systemd/system/volvisor-witnessd.service` | from `volvisor-witnessd.service` here |
| `/etc/volvisor/volvisor.toml` | from `examples/volvisor.toml` (annotated; must pass its own `--check-config` — a repository test pins that) |
| `/etc/volvisor/witnessd.toml` | from `examples/witnessd.toml` (same discipline) |
| `/etc/volvisor/volvisor.env` | the operator's `EnvironmentFile` (optional; the units spell it with `-`, absent is fine) |
| `/var/lib/volvisor/`, `/var/lib/volvisor-witness/` | created by the units' `StateDirectory=` at start, not by the package |

The `RuntimeDirectory=`/`StateDirectory=` directives are why this
package ships **no tmpfiles.d rule**: systemd creates `/run/volvisor`
and `/var/lib/volvisor` (and the witness tree) in the unit's own
namespace at start, with the right ownership, and makes them
writable under `ProtectSystem=strict`. The one runtime directory the
daemon depends on that it does not own — LVM's `/run/lock/lvm` — is
lvm2's own tmpfiles.d responsibility, not ours.

## The readiness contract

Both units are `Type=notify`. The daemons send `READY=1` — and only
that — over `$NOTIFY_SOCKET` when, and only when, their startup is
actually complete:

- `volvisord`: journal lock + replay, provider construction and its
  startup reconcile, **and** the API listener bound;
- `volvisor-witnessd`: witness journal open (lock + replay) **and**
  the listener bound.

Every failure path before those points returns without notifying, so
the service manager sees a start failure at its own timeout and
applies `Restart=on-failure` — which re-enters the same idempotent
startup path (replay and reconcile are idempotent by design; that is
the P6 crash discipline). A *send* failure inside the daemon (e.g. a
removed socket) is logged as a structured warning and never fails
startup: under `Type=notify` the manager's timeout is the operator's
signal, and killing a serving daemon over a notification hiccup
would trade availability for no safety.

The implementation is a hand-rolled `sd_notify(3)` client
(`crates/volvisord/src/notify.rs`) with no new dependencies: one
UNIX datagram, `READY=1\n`. An `@`-prefixed `$NOTIFY_SOCKET` is an
abstract-namespace socket spelled with the NUL substitution the
protocol defines. An unset `$NOTIFY_SOCKET` is a documented no-op —
standalone runs are unaffected. The placement is pinned from both
sides in `crates/volvisord/tests/notify_e2e.rs`: a daemon that fails
reconciliation never reports ready, a daemon that is reconciled but
cannot bind never reports ready, and only the fully-started daemon's
`READY=1` datagram exists.

## The write-set research (behind `ReadWritePaths=`)

Under `ProtectSystem=strict` the entire hierarchy is read-only
except `/dev`, `/proc`, `/sys` and the explicitly listed paths, so
the list must cover the external tools' writes, not just the
daemon's own state. The discipline, honestly stated (review finding
F-1): **load-bearing paths are listed unconditionally** — a missing
one should fail the unit loudly at start — while **cache/hints
paths are best-effort with the `-` prefix**: they may not exist on
fresh installs, and losing them degrades to tool warnings, never to
a failed mutation. The list is researched against the tools'
observed behavior on the reference host, not a claim of exhaustive
coverage of every write any distro build might attempt.

- `/var/lib/volvisor`, `/run/volvisor` — the daemon's journals,
  provider state, migration/handoff records, and the runtime socket
  tree (the VMM `--api-socket` convention) respectively.
- `/etc/lvm/archive`, `/etc/lvm/backup` — LVM 2.03 writes a metadata
  archive and a backup on **every VG metadata change** (`lvcreate`,
  `lvremove`, `pvmove`, `vgextend`, ...). These are load-bearing:
  without write access every LVM mutation fails. Both directories
  ship with the `lvm2` package (verified present on the reference
  host, LVM 2.03.16), so they are listed without the `-` prefix.
- `-/etc/lvm/cache` — the LVM device cache (`write_cache_state`);
  commands may rewrite it when device state drifts. The directory
  does not exist on a fresh install (verified: absent on the
  reference host until first written), hence the `-` prefix.
- `-/run/lvm` — LVM's runtime state (`hints`, `pvs_online`,
  `vgs_online`), opened `O_RDWR` on every mutation per LVM 2.03
  (strace-verified in review). Read-only access degrades to
  cache/hints warnings — non-fatal — but the write is real, so the
  path is listed. `-` because lvm creates it at first use.
- `-/run/lock/lvm` — LVM's global lock directory, created by lvm2's
  own tmpfiles.d at boot; `-` for the same reason.
- `-/etc/drbd.d` — where the DRBD provider writes its resource
  configs (the `drbd_config_dir` default). Present exactly when
  `drbd-utils` is installed, hence `-`.
- `-/var/lib/drbd`, `-/run/drbd` — drbd-utils state/runtime
  directories, included conservatively (drbd-utils is not installed
  on the reference host, so these could not be observed directly;
  the cost of including them if unused is nil).
- `-/sys/bus/rbd` — the ceph provider's attach path maps images
  (`rbd map`), which writes `/sys/bus/rbd/add`: a kernel interface,
  not a filesystem write, that `ProtectKernelTunables`'s read-only
  `/sys` would otherwise block. `ReadWritePaths` re-widenes the
  subtree over the read-only mount (the mechanism is verified in
  the P7-B smoke below; the rbd-specific behavior needs a real
  cluster and is P7-C's). `-` because the path exists only where
  the rbd module is loaded.
- The ceph provider's other children (`ceph`, `rbd` CLIs:
  create/remove/ls/info) are cluster-side metadata operations with
  no local filesystem writes (review finding F-6) — no functional
  gap in the write set beyond the sysfs carve-out above.
- `/proc/drbd` is read, never written — readable under strict
  without a listing.

`RestrictAddressFamilies` keeps `AF_NETLINK` for the storage daemon:
`drbdsetup` administers the kernel module over generic netlink, and
LVM builds with libudev integration use kobject-uevent netlink
sockets (the reference host's LVM 2.03.16 defaults
`obtain_device_list_from_udev = 0`, but distro builds vary — keeping
the family is the conservative choice). The witness runs no external
tools, so its unit drops `AF_NETLINK` and can also take
`PrivateDevices=true`.

Directives deliberately omitted (root/device reality), and the
reasoning, are documented in the header comment of each unit — never
silently weakened.

## The verification boundary

What is verified here:

- `systemd-analyze verify` on both units (syntax and directive
  validation), recorded in the P7-B delivery record;
- the readiness ordering, failure discipline and protocol payload
  pinned by the notify unit tests and the `notify_e2e` integration
  tests above;
- a one-time **unit-start smoke on this host's real systemd** (the
  development sandbox runs systemd 255.4 as PID 1 — review finding
  F-3 corrected an earlier claim here that it did not): the built
  binaries staged at `/usr/bin`, the units and a valid
  configuration installed at their real paths, `daemon-reload`,
  `start`, the unit observed reaching **active** through the
  `Type=notify` READY handshake, the API answering, stop, and full
  cleanup. The full transcript lives in the PR body (it is a
  delivery-time observation, not a permanent test — the packaged
  layout and its smoke belong to P7-C). The smoke also verified the
  `ReadWritePaths`-over-`ProtectKernelTunables` re-widening
  mechanism that the ceph `rbd map` carve-out relies on.

What is **not** verified here, on purpose: the packaged install
itself (P7-C's `.deb`/`.rpm` container smoke: units at their package
paths, `ExecStartPre` against the shipped example configs, restart
cycling, upgrade paths), and provider-specific runtime behavior that
needs real hardware or clusters (the `rbd map` carve-out against a
live ceph host, DRBD against real peers). What cannot be sandboxed
there is recorded, not skipped.
