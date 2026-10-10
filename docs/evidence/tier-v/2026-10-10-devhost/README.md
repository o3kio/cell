# Tier V — the first recorded host run (2026-10-10, devhost)

**What this is.** The P8-A completion gate's recorded real-VMM run
([ADR-0010](../../../adr/0010-tier-v-real-vmm-verification.md)): the
four bounded Tier V scenarios driven against live Cloud Hypervisor
processes at commit `e8b092d` (PR #27). `records/` holds the run's
own evidence records, unmodified; `REPORT.md` is the campaign summary
rendered over the same run.

**What is checkable from this directory alone — and what is not.**
The committed bundle's SHAPE is validated from any fresh checkout by
the default suite: `crates/volvisor-campaign/tests/committed_evidence.rs`
parses every committed record and asserts the invariants that make
the bundle meaningful — tier, the outcome vocabulary, the version +
SHA256 stamps, the expected scenario set, and this README's
per-scenario table agreeing with the records (outcome and duration).
The full certification (`campaign-summary --all --check`, all five
completion gates) ran over the complete run TREE — 103 records
including the Tier S/R matrix — which lives in the gitignored
`target/campaign-evidence/`, not here: CG1 legitimately requires the
full matrix, so a committed Tier V directory alone cannot and must
not pass it. Reproducing the full certification means re-running the
suite (see below).

**What this host is — and is not.** A development sandbox with KVM
(`/dev/kvm`), running the host's own kernel (`6.8.0-142-generic`) and
initramfs as the guest's direct-kernel boot, Cloud Hypervisor
`v43.0.0` at `/usr/bin/cloud-hypervisor`
(sha256 `a250a9347d0ea9e93f88b54b25df3cdc6a9ba3c57f292aaf74bb664fb5c87496`)
and `ch-remote v51.1`
(sha256 `a1a978e16bdb4ec762e92cab00f4983f9ce722d7b191ac3a40f102feafcd9713`).
It is **not** a production host and this is **not** real-media
evidence: the storage below the VMM seam is the campaign's simulated
DRBD worlds (file-backed sparse disks at the declared device paths),
there is no real replication engine and no real host durability —
exactly what every record's `components` states. The claim this
evidence backs is exactly the tier's vocabulary: *verified against a
real VMM process at recorded versions*.

**How it was run.**

```sh
VOLVISOR_TEST_VMM=1 \
VOLVISOR_TEST_VMM_CH_REMOTE=/var/lib/chv/qual/installsh-root/usr/local/bin/ch-remote \
cargo test --workspace --locked
```

1014 tests, 0 failures, at the recorded commit. The Tier V scenarios
boot real 512 MiB guests (direct kernel boot to a busybox shell on
ttyS0 — the guest issues no disk I/O; the write workload is the rig's
model through the simulated device, recorded in every run record).

**Per-scenario outcomes** (the records are the authority; this is a
reading — the shape test pins this table to the records):

| Scenario | Outcome | The real observation |
|---|---|---|
| `migration-happy-path` | complete (1767 ms) | the full protocol over two live VMMs; source VM destroyed, destination VM resumed — both observed over the api sockets; the real snapshot artifacts verified on disk |
| `kill-window-mid-restore` | recovered (1764 ms) | the destination daemon died inside the cut's restore act; the kill leg observed the source already destroyed and the destination VM mid-flight (Running — the blocking restore+resume completed before the abort); the startup pass re-drove to COMPLETE with no double epoch |
| `snapshot-restore-divergence` | complete (1837 ms) | 14 acknowledged writes byte-verified at the destination, the boundary at the REAL pause (stop reason `paused`, zero barrier skew), and both divergent restores refused typed (`InvalidState`) |
| `resize-notification` | complete (60 ms) | both version-gate legs probed from the real binary; the notification took the `retry_required` branch (see the finding below) and the retry pass honestly re-reported it |

**Findings recorded by this run** (honest outcomes, not failures):

1. **`vm.resize-disk` does not exist in Cloud Hypervisor v43.0.0.**
   The real binary answers the notification's `PUT` with HTTP 404
   (upstream added the route by v51.0.0). The scenario asserts the
   contract both ways: a 2xx means `notified`; a typed refusal means
   a durable `retry_required` record carrying the refusal, the grow
   still completes, and the retry pass converges. The record names
   the branch it observed. Re-running the scenario on a host whose
   Cloud Hypervisor carries the route (upstream by v51.0.0), so the
   `notified` leg is recorded from a real observation too, is a
   recorded P8-A follow-up in the
   [implementation plan](../../../plans/2026-10-10-post-p5-implementation-plan.md).
2. **A live empty VMM's `info` answers HTTP 500 with the body "VM is
   not created"** — a not-found shape no doc-derived token matched,
   so the empty-restore-target steady state surfaced as a typed
   INTERNAL instead of `Absent`. Fixed in this PR
   (`NOT_FOUND_TOKENS` in `crates/volvisor-provider/src/vmm.rs`);
   found by the substrate's own environment probe.

**The pair's interop note.** This host's `ch-remote` (v51.1) is
newer than its `cloud-hypervisor` (v43.0.0); the wire shapes the
adapter uses (`vm.info`, `vm.pause`, `vm.snapshot`, `vm.restore`,
`vm.resume`, `vm.delete`, `vm.resize-disk`'s typed 404) were verified
empirically against this exact pair before the scenarios were
written, and the versions + SHA256 above are re-stamped by every run
record — the evidence names the binaries it actually drove.
