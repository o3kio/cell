# Tier V — the second recorded host run (2026-10-11, devhost, Cloud Hypervisor v51.1)

**What this is.** The recorded P8-A follow-up, resolved
([ADR-0010](../../../adr/0010-tier-v-real-vmm-verification.md)): the
[first run](../2026-10-10-devhost/) drove the four bounded Tier V
scenarios against this host's Cloud Hypervisor v43.0.0 and recorded
the resize scenario's *refusal* branch — v43.0.0 predates upstream's
`vm.resize-disk` route. This run drives the same four scenarios
against the **v51.1 upstream release binary** and records the
*notified* branch from a real observation: the real
`PUT /api/v1/vm.resize-disk` answered 2xx and the grow converged
`notified`. `records/` holds the run's own evidence records,
unmodified; `REPORT.md` is the campaign summary rendered over the
same run tree (the full Tier S/R matrix plus this gated run, all at
commit `1762a9d`).

**The pinned binary.** `cloud-hypervisor v51.1` at
`/var/lib/volvisor-test-assets/cloud-hypervisor-v51.1` (outside the
repository; never committed), sha256
`a153dafae80bae92545c4a9165782454b6c03958dae5b128ca9b6e9f5102397d`,
downloaded from the upstream release asset
`https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/v51.1/cloud-hypervisor-static`.
Upstream publishes no digest file of its own for the asset; the
cross-check is the release's platform-published asset digest
(GitHub's release API reports `sha256:a153dafe…2397d` for
`cloud-hypervisor-static`), which matches the computed SHA256 — that
agreement is this run's pin. The adapter binary is the same
`ch-remote v51.1` as the first run (sha256
`a1a978e16bdb4ec762e92cab00f4983f9ce722d7b191ac3a40f102feafcd9713`,
itself byte-identical to upstream's `ch-remote-static` v51.1 asset).

**What is checkable from this directory alone — and what is not.**
The same discipline as the first bundle: the committed shape is
validated from any fresh checkout by
`crates/volvisor-campaign/tests/committed_evidence.rs` (which
discovers every bundle under `docs/evidence/tier-v/`), and the full
certification (`campaign-summary --all --check`, all five completion
gates) ran over the complete run TREE — 103 records including the
Tier S/R matrix — which lives in the gitignored
`target/campaign-evidence/`, not here.

**What this host is — and is not.** The same development sandbox as
the first run: KVM (`/dev/kvm`), the host's own kernel
(`6.8.0-142-generic`) and initramfs as the guest's direct-kernel
boot, file-backed sparse disks at the declared device paths (the
storage below the VMM seam is the campaign's simulated DRBD worlds —
every record's `components` states it). Not a production host, not
real-media evidence; the claim this evidence backs is exactly the
tier's vocabulary: *verified against a real VMM process at recorded
versions* — here, a matched v51.1 pair (the VMM and the adapter
binary are the same generation, unlike the first run's v43/v51
mixed pair).

**How it was run.**

```sh
VOLVISOR_TEST_VMM=1 \
VOLVISOR_TEST_VMM_CLOUD_HYPERVISOR=/var/lib/volvisor-test-assets/cloud-hypervisor-v51.1 \
VOLVISOR_TEST_VMM_CLOUD_HYPERVISOR_LEGACY=/usr/bin/cloud-hypervisor \
VOLVISOR_TEST_VMM_CH_REMOTE=/var/lib/chv/qual/installsh-root/usr/local/bin/ch-remote \
cargo test -p volvisor-campaign --test tier_v
```

after a fresh full skip-mode suite at the same commit (1046 tests,
0 failures). The `LEGACY` override claims the host's v43.0.0 binary
for the resize scenario's version-gate boundary leg — its
`--version` is probed through the real runner, never a launched VMM
(the record's `components` names it with its own stamps and role).

**Per-scenario outcomes** (the records are the authority; this is a
reading — the shape test pins this table to the records):

| Scenario | Outcome | The real observation |
|---|---|---|
| `migration-happy-path` | complete (1347 ms) | the full protocol over two live v51.1 VMMs; source VM destroyed, destination VM resumed — both observed over the api sockets; the real snapshot artifacts verified on disk |
| `kill-window-mid-restore` | recovered (1450 ms) | the destination daemon died inside the cut's restore act; the kill leg observed the mid-flight state over the api sockets; the startup pass re-drove to COMPLETE with no double epoch |
| `snapshot-restore-divergence` | complete (1738 ms) | the acknowledged writes byte-verified at the destination, the boundary at the REAL pause, and the divergent restores refused typed over the real api socket |
| `resize-notification` | complete (83 ms) | the version gate at the route's boundary (51.0.0) proven against the real v51.1 binary — the notification reached the real VMM and **landed (a 2xx from the real `vm.resize-disk` route)**; the refused control (99.0.0); the same 51.0.0 boundary unproven against the real legacy v43.0.0 binary — fail-closed `retry_required` before any VMM call; and the retry pass |

**The empirical bounds now known** (two live-verified points, stated
no wider):

1. **Cloud Hypervisor v43.0.0 lacks `vm.resize-disk`** — the real
   binary answers the notification's `PUT` with HTTP 404 (the first
   run's recorded refusal branch).
2. **Cloud Hypervisor v51.1 carries it** — the real route answered
   2xx and the notification converged `notified` (this run's
   record). Between the two live-verified points, upstream's own
   pinned OpenAPI places the route's introduction: v49.0's schema
   has no `VmResizeDisk`, v50.0's carries it — a document-verified
   bound, distinct from the live observations at its endpoints.
3. **The route's request body is `{"id", "desired_size"}`** (bytes)
   per v51.1's published OpenAPI — verified live: the first probe
   with the pre-`desired_size` field name was refused with a typed
   400 (`missing field desired_size`), and the corrected body
   completed. The adapter's pinned request bytes follow this shape
   (PR #32).
4. **The version gate's boundary, recorded both ways**: the same
   51.0.0 minimum — below the live-verified route-carrying binary,
   above the live-verified route-absent one — proven against v51.1
   (the notification proceeds) and unproven against the real v43.0.0
   binary (fail-closed `retry_required` before any VMM call, the
   gate's reason in the durable record).

**Findings recorded by this run** (honest outcomes, not failures):

1. **The API-drift finding**: v51.1's live route refused the
   adapter's original body (the pre-route field name) with a typed
   400 naming the missing field — exactly the drift the v43 run's
   404 could never surface (a route-absent answer parses no body).
   The body was corrected to the pinned OpenAPI shape and the
   pinned-bytes tests follow the real serialization. Recorded here
   and in PR #32; the first run's record remains an accurate
   observation of v43.0.0.
2. **The empty-VMM `info` shape on v51.1**: the same "VM is not
   created" answer as v43, wrapped in v51's richer error chain —
   the pinned `NOT_FOUND_TOKENS` matched both generations
   unchanged; no adjustment was needed (verified empirically
   against both binaries before the run, not assumed).
