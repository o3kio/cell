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
the list must cover everything the daemon **and the external tools
it spawns** write — not just the daemon's own state:

- `/var/lib/volvisor`, `/run/volvisor` — the daemon's journals,
  provider state, migration/handoff records, and the runtime socket
  tree (the VMM `--api-socket` convention) respectively.
- `/etc/lvm/archive`, `/etc/lvm/backup` — LVM 2.03 writes a metadata
  archive and a backup on **every VG metadata change** (`lvcreate`,
  `lvremove`, `pvmove`, `vgextend`, ...). These are load-bearing:
  without write access every LVM mutation fails. Both directories
  ship with the `lvm2` package (verified present on the reference
  host, LVM 2.03.16), so they are listed without the `-` prefix — a
  missing directory should fail the unit loudly at start, not every
  mutation later.
- `-/etc/lvm/cache` — the LVM device cache (`write_cache_state`);
  commands may rewrite it when device state drifts. The directory
  does not exist on a fresh install (verified: absent on the
  reference host until first written), hence the `-` prefix.
- `-/run/lock/lvm` — LVM's global lock directory, created by lvm2's
  own tmpfiles.d at boot; `-` for the same reason.
- `-/etc/drbd.d` — where the DRBD provider writes its resource
  configs (the `drbd_config_dir` default). Present exactly when
  `drbd-utils` is installed, hence `-`.
- `-/var/lib/drbd`, `-/run/drbd` — drbd-utils state/runtime
  directories, included conservatively (drbd-utils is not installed
  on the reference host, so these could not be observed directly;
  the cost of including them if unused is nil).
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
  tests above.

What is **not** verified here, on purpose: the units' runtime
behavior under a real systemd — namespace setup, `StateDirectory`
interaction with `ProtectSystem=strict`, restart cycling, `Type=notify`
timeouts. No systemd runs as PID 1 in the development sandbox. That
boundary belongs to P7-C's container install smoke (unit start →
active, readiness observed), per the plan's own evidence shape; what
cannot be sandboxed there is recorded, not skipped.
