# ADR-0011 — The local-mirror experiment: declined (P8-C)

Status: Accepted (the decision is the deliverable — a decline with a
recorded reason and a reopenable sketch; no capability is claimed and
none is built)
Date: 2026-10-10
Related: [nearline contract](../../contracts/nearline-replication-v2.md)
§3/§10 (the mirror states and the ledger line this decision closes),
[SPEC-0002](../specs/SPEC-0002-volvisor-volume-virtualization.md) §12
(the P3 residue this decides),
[ADR-0007](0007-drbd9-nearline-replication-provider.md) (R3 — the
fault classes; the alternatives table),
[ADR-0006](0006-online-resize-and-live-local-block-relocation.md)
(the reserved design space a mirror would return through),
[implementation plan](../plans/2026-10-10-post-p5-implementation-plan.md)
stage P8-C.

## Context

The nearline §10 ledger has carried one honestly-undecided residue
since the P5 campaign: *"Local mirror leg loss/repair/rebuild — no
mirror implementation exists to fault; the gate is double-recorded
(hardware **and** implementation) as `tier-r/mirror-leg-loss`."*
SPEC-0002 §12's P3 row pointed that residue forward to P8, and the
implementation plan's P8-C offered exactly two honest outcomes:
design the experiment and fault it on the real-host/media tier, or
decline it with a recorded reason. This ADR records the decline.

## Decision

**Decline the local-mirror experiment.** No mirror is designed, built
or faulted in this phase, and the decline is a decision, not a
deferral. The reasons:

1. **Nothing exists to fault, and nothing authorizes building one.**
   No local-mirror implementation exists in the tree, and no accepted
   contract or ADR acceptance suite authorizes one: the nearline
   contract specifies mirror *states* (leg health, rebuild progress)
   as provider surface should a mirror exist, but no class, no
   consumer contract and no ADR makes a local mirror part of any
   delivered class's obligations.

2. **The durability need is already carried by design.** Nearline
   durability is cross-host replication (the nearline v2 contract's
   inter-host failure domains; two local legs never count as remote
   replicas). A local mirror adds same-host device redundancy whose
   value proposition — surviving single-device loss *without*
   cross-host failover — is a new availability tier no consumer has
   contracted for. Building an uncontracted availability tier to
   have something to fault would invert the evidence discipline this
   repo runs on.

3. **The validating fault classes only exist on real media.**
   ADR-0007's R3 (single leg loss, repair, rebuild contention; media
   failure must not invent authority) can only be honestly faulted
   on real devices. The delivered tiers prove nothing there: Tier S
   proves logic (its storage is a block map with no device granule —
   a separate-SSD loss fault cannot even be injected below it), and
   the Tier V devhost proves the VMM seam with **simulated storage
   below the seam**. A mirror faulted anywhere in those tiers would
   be a simulation of a media behavior, which is precisely what the
   tier vocabulary forbids claiming.

4. **The design space where a mirror could return is already
   reserved.** ADR-0006 gates any mirror/pivot-era capability behind
   its acceptance suite (and its Option C language already warns
   against hand-rolled block copiers for exactly the correctness
   reasons a mirror would face). A future local-mirror slice would
   enter through that gate — whose suite already contains the
   mirror-relevant cells (4K random writes during mirror, source SSD
   failure) — without requiring the QSD pivot itself, not through
   this residue.

## The reopenable sketch

Recorded so a future operator can reopen without re-deriving. None of
this is built, tested or claimed; it is the shape the experiment
would take.

**The mirror layout**: an LVM mirror (`lvconvert --type mirror` /
raid1) or DM-mirror below one nearline replica, legs on **separate
physical devices** (the nearline contract's rule that legs live on
distinct devices is what makes leg loss a device-level fault, not a
device-multiplexed one). Volvisor treats the mirror as a distinct
resource with independent identity, generations and ownership
(AGENTS rule 1): leg count, healthy legs, and rebuild progress are
the provider surface — the nearline §3 `mirror` states the contract
already names.

**The fault classes and their evidence shapes** (ADR-0007's R3,
extended by the contract's own protections):

- *Single leg loss*: fail one device (real media — the failure is
  the device, not a simulated bit). Evidence: the volume stays
  writable (the surviving local leg serves), **no cross-host writer
  election fires** (the contract's rule: local leg loss must not
  trigger a cross-host election while the active local copy is
  valid), the degraded state is exposed (`mirror: legs=2 healthy=1`),
  and a second failure during the degraded window is refused or
  fenced, not silently absorbed.
- *Repair/rebuild*: restore the leg; evidence: rebuild progress is
  observable and bounded, foreground guest I/O is rate-isolated
  from the rebuild stream (the contract's three-stream rule), and
  the volume never advertises healthy before the rebuild is real.
- *Rebuild contention under peer loss*: simultaneous peer loss
  during a local rebuild (R3's hardest cell); evidence: the
  coordinator's classification (which failure dominates), no
  invented DRBD/volume authority from a media error, and the
  acknowledged-write set survives exactly what the declared
  contracts promise.
- *Media failure does not invent authority*: a failing leg's I/O
  errors must surface as degraded redundancy, never as a silent
  promotion or a quorum opinion.

**What the real-host/media tier would have to run**: the existing
`tier-r/mirror-leg-loss` gate's scenario shape on real devices — the
Tier R drive with a mirror-backed replica, the faults above injected
at the device level, the campaign's record discipline (exact
versions, per-scenario records, honest not-run vs not-implemented),
and the media-flush/FUA and power-cut gates (`tier-r/media-flush-fua`,
`tier-r/power-cut`) green on the same hardware, because a mirror's
promises are only as good as the media semantics beneath it.

## Reopen condition

The experiment reopens when **a consumer contracts for the
availability tier** (single-device loss survivable without
cross-host failover) **and a mirror design passes ADR-0006's
acceptance-suite gate** on real media — through the suite's
mirror-relevant cells, without requiring the QSD pivot itself. Until
both hold, the nearline §10 ledger's mirror line records this
decline, and the `tier-r/mirror-leg-loss` gate remains the honest
record of what was not run.
