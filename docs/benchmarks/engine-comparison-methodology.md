# The engine-comparison benchmark methodology and its decision gate (P8-B)

Status: normative methodology (reviewed like any normative doc); the
decision gate it defines is **recorded closed** — 2026-10-10, PR #28
(no measurements exist that could carry a build decision; §7 records
the state exactly)
Date: 2026-10-10
Related: [SPEC-0002](../specs/SPEC-0002-volvisor-volume-virtualization.md)
§12 (the consideration item this methodology serves),
[ADR-0007](../adr/0007-drbd9-nearline-replication-provider.md) (the
alternatives table whose NO-GO this gate keeps honest, and evidence
rows R2/R6), [R&D comparison](../research/replicated-async-rnd.md)
(candidates B/C — the architecture study that does NOT authorize
anything by itself), [nearline contract](../../contracts/nearline-replication-v2.md)
§2/§4 (the ACK semantics and the local-ACK minimum every candidate is
held against) and §10 (the benchmark ledger row),
[ADR-0009](../adr/0009-distribution-and-packaging.md) (the packaging
that makes installable pinning possible),
[ADR-0010](../adr/0010-tier-v-real-vmm-verification.md) (the evidence
discipline the benchmark runs inherit), [implementation
plan](../plans/2026-10-10-post-p5-implementation-plan.md) stage P8-B.

## 1. Purpose — what this gate is

SPEC-0002 §12's engine-comparison consideration gate exists to answer
one question with measurements: **should Volvisor build a new
replication engine (a clean io_uring/SPDK engine, or an adopted
Mayastor io-engine data plane) instead of DRBD 9 for the nearline
class?** ADR-0007's alternatives table already recorded the default
answer — the clean engine is **NO-GO unless DRBD/Mayastor measured
gaps demand it** — and this methodology is the instrument that turns
that conditional into a decided state, either way, on evidence.

The comparison runs **what would actually be deployed**: P7's
packaging (ADR-0009) is what makes this possible. The volvisor side
of every measurement is the released deb/rpm artifact at its exact
SHA, its `volvisord --version` output recorded; the engines under
comparison are installed and configured the way an operator would
install them, not built ad hoc for the bench. A gap measured against
a bench-only build is not a gap in the deployed system.

## 2. The honesty spine

**A benchmark is not a durability proof** (the nearline §10 ledger's
own rule, and AGENTS rule 16). No number in this comparison licenses
a durability claim; durability evidence lives in the failure-campaign
tiers and, for any candidate engine, in its own proof obligations
(redo log, ordering, crash replay, fencing, migration safety —
ADR-0007's mismatch column: a new engine *owns* all of them).

**Never imply apples-to-apples from different ACK semantics.** Every
engine's durability contract is stated alongside its numbers, and
cross-engine conclusions respect those boundaries:

| Engine | What an ACK means | When data is on stable storage | What a failover loses |
|---|---|---|---|
| DRBD 9 Protocol A (`async`, the v2 default) | local disk write complete AND handed to the local TCP send buffer | the local replica's media (the peer's copy is eventual) | acknowledged writes survive on the surviving host; the peer may lag by the in-flight window |
| DRBD 9 Protocol B (`semi-sync`) | local disk complete AND remote memory arrival | local media; remote memory (not remote media) | as A, with a smaller in-flight window |
| DRBD 9 Protocol C (`sync`) | local AND remote disk completion | both replicas' media | only unacknowledged writes |
| Mayastor io-engine (replicated Nexus, as upstream) | writes sent **synchronously** to healthy children before ACK | per-child policy (NVMe local; NVMe-oF remote) | a synchronous-tier contract — a *different* durability class than the v2 default, whatever the numbers say |
| Clean io_uring/SPDK engine (hypothetical) | whatever its design says — this is precisely the cost of building it | by its own redo-log/replay proof | by its own proof |

The comparison's primary contract is the **v2 default async profile**
(local-ACK minimum per nearline §4: an ACK means local stable
storage; acknowledged writes are never lost to a single-host
failure). Numbers from a stronger synchronous contract are reported
**labeled cross-contract** and cannot by themselves justify a build:
a candidate that wins by acknowledging less must say so.

## 3. Topology and the pin list

**Matched hardware.** The same host model, the same NVMe SKUs
(firmware versions recorded), the same kernel on both hosts of a
nearline pair; a third diskless quorum witness host (the nearline
topology the campaign already models). Cross-host measurements run
on the same switch and NICs, recorded. A comparison across unlike
hardware is not a comparison; it is two anecdotes.

**The pin list** (recorded per run, in the run's records — §5):

- the volvisor deb/rpm artifact: package version, artifact SHA256,
  and the installed `volvisord --version` / `witnessd --version`
  output;
- DRBD 9: kernel module version and source SHA (the kmod is a host
  property the volvisor package cannot install — ADR-0009 — so the
  pin is the operator's recorded installation, verified by
  `/proc/drbd` version and module signature);
- Mayastor / SPDK components under test: each component's version
  and source SHA (the R&D doc's provenance rule: architecture study
  does not authorize copy/translation; any reuse needs its own
  license review);
- cloud-hypervisor: version and file SHA256 (the Tier V stamp
  discipline), plus `ch-remote` when driven;
- the kernel: `uname -r` and the flush/FUA-relevant configuration;
- fio: version, job files committed with the run.

## 4. The workload matrix

**Primary metrics** (the gate's §6 criteria read these): IOPS,
bandwidth, and latency percentiles (p50/p99/p999) at **matched queue
depths** (1, 4, 16, 32 — the same depths for every engine, stated
per cell), and **CPU cost per engine** (cores burned at matched
load: a userspace engine that wins p99 by spending two more cores
must carry that number next to the win).

**Workloads**: 4K random read/write, 128K sequential read/write, and
the flush/FUA variants of the write cells (strong guest flush and
crash consistency are explicitly tested requirements — AGENTS rule
11 — and the flush path is where engines differ most); the mixed
steady-state cell (foreground writes while a resync/rebuild runs —
ADR-0007's R2 shape: backlog and convergence under guest load).

**Operational behaviors** (not microbenchmarks, reported alongside):
failover/reconvergence time after peer loss, rebuild rate under
foreground load, and the backlog bounds — the behaviors that decide
whether an engine is operable, whatever its peak IOPS.

**Guest realism**: the workloads run through a real VMM disk (the
Cloud Hypervisor host block frontend, the Tier V composition), because
the engine's cost position includes the frontend path. Comparing
kernel-DRBD-through-CHV against a userspace engine through a
different frontend must measure the real paths as deployed, with the
frontend named in each cell.

## 5. Run discipline

Benchmark runs inherit the campaign's evidence discipline (the Tier
V/Tier R shapes):

- a **run directory** with per-scenario JSON records carrying the
  full pin list (§3), the workload cell, the raw metric extract, and
  the engine's stated durability contract (§2) — a record that does
  not name its binaries at exact versions is not evidence;
- the outcome vocabulary is honest: **not run** is recorded
  distinctly from **not implemented**, and a claimed environment
  that cannot back the claim fails loudly before any record is
  written (the env-gate discipline);
- `REPORT.md` rendering from the records alone, reproducible by the
  standalone summary tooling;
- the fio job files, configs, and per-engine setup scripts are
  committed with the run so the numbers are re-derivable.

## 6. The decision gate

A new engine is justified **only** when all of the following hold,
measured by this methodology on matched hardware:

1. **Magnitude, sustained**: a **≥2× factor** (not a percentage — a
   from-scratch engine's cost is dominated by correctness work, so
   only a step-change pays for it) on the primary metrics — p99
   write latency at matched queue depth for the default async
   profile, and/or IOPS at a matched p99 budget — **across the full
   workload matrix**, on both hosts of the pair, not in one
   cherry-picked cell. A single-cell win is a tuning observation.
2. **Durability boundary**: the candidate's ACK contract is at least
   the v2 default's local-ACK minimum, stated and **verified** by
   the failure-campaign evidence a benchmark alone cannot substitute
   for (flush/FUA with monotonic-checksum verification; power-cut
   evidence at the media tier). A candidate that only wins by
   acknowledging less fails here.
3. **No operational regression**: failover/reconvergence and rebuild
   under foreground load within DRBD 9's envelope or better, with
   the CPU cost stated per §4.
4. **The gap survives tuning**: the DRBD configuration space
   (protocol choice at the same contract, batching, bitmap
   settings) is explored and recorded — the multiple must be
   against the **best-tuned** DRBD, not the default configuration.
5. **Cross-contract numbers are labeled and insufficient**: cells
   whose ACK semantics differ from the primary contract are
   reported as cross-contract and **cannot alone** justify the
   build (§2).

The gate's **opening** is a recorded decision carried by
measurements: the methodology executed per §3–§5, the records
committed, and a decision document citing those records by run id.
Opening by argument, by vendor numbers, or by the R&D doc's
architecture study (which explicitly authorizes nothing) is not a
gate opening.

## 7. The recorded decision (2026-10-10): the gate stays closed

**No benchmark of this methodology can honestly run in this phase.**
The environment available to this work is a single virtualized
development sandbox: it has **no DRBD 9 kernel module**, **no
Mayastor or SPDK installation**, and **no matched hardware** (not
two physical hosts of the same model with the same NVMe SKUs — the
§3 topology does not exist here). Those are the facts; they are
recorded rather than worked around, because a benchmark run on an
unbacked environment would be fabrication, exactly the failure mode
the campaign's env-gate discipline exists to prevent.

**Decision: do not build a new engine now.** The measurements that
could carry a build decision do not exist. ADR-0007's alternatives
table stands unchanged — the clean io_uring/SPDK engine remains
**NO-GO**, now with the gate's state recorded instead of pending;
the Mayastor io-engine remains a serious data-plane candidate whose
adoption would itself require the standalone experiment the R&D doc
describes (io-engine without the Kubernetes control plane), run
under this methodology.

**What would reopen the gate**: executing this methodology on
matched hardware — the §3 pin list installable, the §4 matrix run
through the real VMM frontend, the §5 records committed — and the
results meeting §6's criteria. The reopen condition is the
measurements' existence, not a better argument.
