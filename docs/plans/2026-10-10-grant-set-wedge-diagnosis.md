# The `grant_set` wedge: diagnosis

**Status**: diagnosed 2026-10-10 (P6-A part 3). The defect of record
lives in the P5 campaign's findings (the `grant_set` wedge) and the
readiness plan §1; this document is the precise mechanics — where the
failure journals, why it replays forever, and what a re-issue must
reconcile with. The fix and its safety proofs are PR #23's body; the
campaign rows that reused the park as an injection window are
re-derived there. The convergence horizon this document recorded as a
follow-up (below) is closed by PR #33 — the renewal-during-parks and
gated fresh-grant mechanisms, with their safety statements and honest
boundaries in that section.

## The defect in one paragraph

A witness kill inside the grant commit parks the migration
**permanently** at `destination_authorized` (observed with a stall
detail): the destination's peer-grant operation journaled its failure,
and the ops pipeline replays recorded outcomes — success or failure —
byte-for-byte forever. The park is safe (fail-closed: no dual writer,
the source stays fenced, the destination never promotes) but recovery
requires operator action: the post-revoke record has no rollback
(abort refuses cut-or-later, G1/D1a) and no consumer recourse (the
derived operation id is deterministic per migration, so every re-drive
of the promote step hits the same recorded failure).

## The mechanics, step by step

1. **Where the failure journals.** The source's drive reaches
   `drive_authorize`: the target is not granted, so it calls the
   destination's `POST /v2/internal/peer/grant` (the `grant_set`
   step). The route runs through the ops pipeline
   (`execute_resolvable`): the intent is journaled (the derived id
   `mig-api-peer-grant-…`), then `grant_act` executes — the witness
   batch grant under its deterministic operation id, then the
   per-participant promotes with their device paths recorded durably
   in the preparation. A witness kill inside the batch commit kills
   B's witness connection mid-call: `grant_act` returns the transport
   error, and the pipeline's failure tail (`finish_outcome` →
   `append_outcome(success=false, error_body)`) durably records that
   failure under the same derived id. B survives; the failure is on
   record.

2. **Why it replays forever.** The replay rule
   (`execute_resolvable` step 2): a recorded outcome for the same
   operation id and request hash is replayed byte-for-byte *without
   executing* — with no distinction between success and failure. The
   idempotency-registry semantics are deliberate (a failed act is
   never blindly re-executed), but for an act whose failure was
   *transient* — the witness came back — the recorded answer is a
   fact about the past attempt, not about the world. The source's
   retry task re-drives every 5 s tick: the fold lands the record at
   `destination_authorized` (the witness's own journal replay
   re-derived the grant from its durable intent — W3), the drive's
   promote step re-calls B's grant route for the promoted device
   paths, and the route re-serves the recorded 500. Forever. The
   retry is a bounded spin (never a silent resume) — the honest
   reading is a permanent park.

3. **What actually landed (the partially-landed state a re-issue
   must reconcile with).** The world is *almost* converged at the
   moment of the park:
   - **Witness**: the grant **did** land — the witness's journal
     replay re-derives it from the durable intent, so epoch 2 is live
     and held by the destination. (The two armed kill windows both
     leave the intent durable; the row-7 cells prove the replay
     lands the grant exactly once.)
   - **Source**: fully fenced — the cut is durable (the VM destroyed,
     every participant demoted, the leases revoked); the fold proves
     it into the record. Nothing on the source side re-acquires
     authority through any peer route.
   - **Destination (B)**: the *missing* pieces are exactly the parts
     the failed act never reached: the per-participant promotes
     (idempotent per migration — a completed promote replays its
     recorded attachment response) and the promoted device paths in
     the durable preparation (the restore's disk-mapping verification
     needs them). A re-issued grant act must therefore reconcile
     with: a live lease it did not itself mint (the witness batch
     under the same deterministic operation id replays the recorded
     outcome — it cannot mint a second epoch), promotes that may have
     partially landed (idempotent, same attachment identity), and a
     preparation whose device paths may be absent (not provable
     landed → re-execute; recorded → serve).

## Why the remedy is "re-resolvable peer acts"

The readiness plan's remedy direction names two options: re-resolvable
peer acts, or a promote path that does not route through the failed
grant op. The first is the right one: the grant route *already* has
the machinery — `grant_inspect`, the total landed-ness inspection
that resolves the **in-flight** case (an intent without an outcome) by
proving the act landed or did not from durable world state (the
witness views plus the preparation's device paths). The recorded
*failure* case is the same question with one more fact on record: an
attempt that failed. Re-issuing the act through the same inspection
is safe exactly because the act's safety shape holds (the proofs are
PR #23's body): a deterministic derived id, a total inspection, an
idempotent re-execution at every layer (the witness batch under its
op id, the promote per migration, the attach identity deterministic),
and world-derived refusals that reproduce identically on
re-execution — no operator judgment is bypassed.

A promote path that bypasses the grant op would *duplicate* the
inspection's job (proving the grant landed before promoting) in a
second code path, and would leave the recorded failure sitting in the
journal as a landmine for the next same-id caller. Re-resolvability
fixes the class, not the symptom.

## The convergence horizon (the live-lease assumption — CLOSED by the horizon remedy)

The re-issue's convergence originally relied on one world fact: the
witness-side grant remaining live. The minted grant-hold leases carry
the 60 s TTL (`DEFAULT_LEASE_TTL_SECS`), and an unpromoted grant was
never renewed — the destination's renewal pass (`renew_leases`)
iterates only its own volumes (the ones with authority blocks), and a
grant-hold lease for a volume the destination never promoted has
none. A park that outlived the TTL therefore had no re-mint path: the
deterministic operation id replayed the expired grant forever (the
witness journal re-serves the recorded grant outcome), and
`promote_target` refused over the non-live lease — the spin stayed
honest and fail-closed, but the migration no longer self-resolved.
That gap was recorded here as a follow-up and is now **shipped** (the
PR that closes this section); the remedy is both recorded directions,
composed:

1. **Renewal during parks (the common case).** The daemon's
   lease-renewal task now renews grant-hold leases alongside volume
   leases: every live preparation on the destination whose
   pre-promote participants (those without a recorded device path —
   the durable proof a promote completed) uniformly hold live leases
   at the witness gets them renewed, on the same `remaining ≤
   renewal interval` due rule the volume pass uses (the witness
   extends from the renewal time, W5). The safety statement: renewal
   creates **no new authority** — the grant remains usable only
   through `promote_target`'s existing epoch/fencing gate; renewal
   only keeps the recorded grant serviceable; renewal **stops at
   terminal shapes** (a promoted participant's lease belongs to its
   volume's own authority block from the promote's completion save
   onward, a removed preparation is not renewed at all — no lease
   outlives its migration record); and a witness unreachable at
   renewal time defers honestly to the next pass (the park keeps
   spinning, unchanged semantics). One coupling is deliberate: the
   pass renews a preparation's pre-promote leases only when **every**
   one is live — a single dead member makes the batch's grant
   un-renewable (`StaleEpoch` cannot revive a lapsed lease) and would
   otherwise block the fresh-grant gate below forever; letting the
   remaining members lapse converges the preparation to the
   uniformly-dead shape within one TTL.
2. **Fresh-grant on a provably-dead lease (the long-outage case).**
   When renewal could not keep the lease alive (the witness — or the
   destination's renewal pass — was down past the TTL) and the world
   has recovered, the grant act re-issues under a **new derived
   operation id**: the attempt counter journaled on the preparation
   (bumped durably before the witness call, so re-drives after
   crashes agree) folded into a reserved-namespace id
   (`derived:mig-grant-set-r{attempt}-{16 hex}`, the PR #31
   discipline; attempt 0 keeps the original bare id byte-identically
   so in-flight journals replay). The re-mint is gated on **all** of:
   (a) the witness authoritatively reports every participant's
   recorded grant lease dead-by-lapse and still ours (state `Expired`
   with this host as holder — never inference from local state; a
   `Revoked` lease is a deliberate end — the destination's own
   fail-closed release or a W6 revocation — and is refused; a live
   lease is renewal's job, and a foreign or absent lease never
   reaches this gate); (b) the record is provably pre-promote (no
   participant records a device path — the re-mint retires the
   granted epoch, and a completed promote's authority block is
   anchored to it: re-minting would fence a legitimate promote, so
   the at/after-promote park is a typed refusal and stays operator
   territory); (c) the witness re-validates the source's fence state
   (the barrier-or-retirement evidence below the granted epoch, the
   same derivation `promote_target` demands — a fresh grant never
   mints while any writer could be live, and the witness's own W7
   fence window past the lease end is the arbiter of when the re-mint
   may land). If any gate fails the refusal is typed and the park
   keeps its honest spin.

With both, a parked wedge heals regardless of park duration as long
as the world recovers — the witness's authority is the arbiter
throughout, no operator action required. The honest boundaries, so
the close is not silent: the **at/after-promote park** (some
participant's promote completed before the outage) refuses the
re-mint (gate b) and remains operator territory — its fully-promoted
participants still converge through the promote replay, but the mixed
park's un-promoted tail needs a lease the re-mint may not mint; the
**revoked-lease park** (row 12b's fail-closed release) is refused
(gate a) — a deliberate end is never silently reversed; and the
**mixed-liveness park** (one member lapsed, another still live)
spins until the still-live member lapses (the renewal's uniform rule
above) and the uniformly-dead gate opens — bounded by one TTL, never
an operator action.

## Review notes (PR #23's review)

Three properties the review asked to have named in this record:

- **Response divergence across callers of one operation id.** The same
  peer-act operation id can serve `INTERNAL` to its first caller and
  `200` to a later retry (the supersede path journals and serves the
  proven outcome); safe today because the only consumer — the source
  daemon's drive — treats peer-act failure as retryable, but a future
  consumer treating peer-act failure as terminal would diverge from
  the later state.
- **The restore act is not internally serialized at the VMM layer**,
  unlike the witness layer's single-guard serialization (the batch
  replays under one operation id). Concurrent restore re-issues
  converge to exactly one fully-restored VM by construction: the
  destroy arm targets only the non-serving `Created`/`Paused` shapes
  (this migration's own half-restore, bounded by the prepare act's
  emptiness proof — the socket was proven empty before the source's
  cut), never a serving VM.
- **The discard advisory field diverges on re-issue**: an
  already-landed discard re-issued serves `discarded:false` (the
  inspection's proven answer — the preparation is absent) where the
  original call served `discarded:true` (it removed a present
  preparation). Idempotent semantics — the preparation is gone either
  way — and metadata-only: the divergence is in the advisory field,
  not in any state the caller acts on.

## The operation-id namespace (the F5 follow-up, resolved)

The review also pinned one defect beyond the wedge itself (F5, the
readiness plan §1's recorded backlog): the derived act ids this
document describes rendered `mig-api-{tag}-{16hex}` in the **same**
journal namespace as consumer-supplied operation ids, and the
derivation is deterministic from the consumer-chosen migration id —
so an authenticated consumer could pre-register the id a migration's
act would derive, with a differing payload, and wedge that act
forever on the journal's `IDEMPOTENCY_CONFLICT` (DoS-class: nothing
mutates on the conflict, but the act never lands either).

**Resolved in PR #31** by namespacing the derivation, not the
journal: every derived id renders in a reserved `derived:` prefix
(`derived:mig-api-{tag}-{16hex}` for the acts, and
`derived:mig-attach-{16hex}` for the promote attach identity, which
doubles as the derived attachment id), and every consumer route that
journals a consumer-supplied id refuses the prefix typed before any
journal write — a refusal under a reserved id must leave no record,
or the refusal itself would become the pre-claim. The derivation
remains a pure function of the migration identity and act tag, so
the re-issue machinery above is untouched: a post-crash re-drive
re-derives the same id and replays the recorded outcome, and the
healed-wedge rows run green at the fix's head. The example ids in
this document (`mig-api-peer-grant-…`) therefore read
`derived:mig-api-peer-grant-…` on any daemon at or past PR #31. No
deployed journal predates PR #31 (the project is pre-production —
ADR-0003 makes no deployed API compatibility claim), so this one-way
rendering owes no replay migration; the note exists for a future
reader comparing this document's examples against real journals.
