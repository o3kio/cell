//! Device lending over claimed devices (the rook-cell contract's
//! lending surface, P9-A).
//!
//! Lending shares a volvisor-claimed device with a named borrower (a
//! cell) **without surrendering the claim**: the claim, the lend and
//! the reclaim are generations of one durable ownership record, so
//! there is never a double-ownership window. The surface lives
//! entirely in the device-claiming layer (readiness plan D3's scope
//! note): it does not touch the volume classes, and no
//! production-support claim is made anywhere in it.
//!
//! ## The ownership record and its journal
//!
//! [`DeviceOwnershipRecord`] extends the claim record with a
//! monotonic `lend_generation` counter, an optional live lend
//! sub-record, an optional quarantine, and — the journaling half —
//! an optional **pending intent**. Every mutation is
//! journal-before-mutate at the record layer: the intent is saved
//! durably first ([`DeviceOwnershipStore::save`], through the
//! [`STORE_DEVICE_OWNERSHIP`] crash seam), then the effect is applied
//! and saved again. A crash between the two leaves the pending
//! intent visible, and a visible pending intent **derives a
//! quarantine** ([`effective_quarantine`]): the device's lending
//! state cannot be determined, so it is not lent, reclaimed or
//! mutated — it is quarantined pending reconciliation, never
//! inferred from observed state. Reconciliation is the explicit
//! [`resolve_quarantine`] operator action; the daemon never guesses
//! an unknown-outcome lend into existence (fail-closed, the opposite
//! of the witness's startup roll-forward, which re-derives whole
//! mutations — here the record's pre-intent state is the only
//! provable one).
//!
//! At the API layer the operations journal through the shared
//! pipeline like every privileged mutation (`operation_id` +
//! immutable request hash, Terminal replay class); an in-doubt retry
//! answers `OPERATION_IN_DOUBT` and never re-executes.
//!
//! ## The release/residue observation seam
//!
//! [`DeviceSurfaceObserver`] is the named seam between the ownership
//! record and the device's durable surface — **what the borrower
//! leaves observable**. Its contract:
//!
//! - one observation per call, over the device's durable surface
//!   (not the ownership record; the record is the engine's own
//!   state);
//! - `BorrowerSignature` = the named borrower's signature is present
//!   (still held, or last held);
//! - `Released` = no borrower signature is observable;
//! - `Foreign` = a signature that is not the lent borrower's;
//! - an error = the surface cannot be observed; the caller fails
//!   closed and writes nothing.
//!
//! In the Tier S world [`FakeDeviceWorld`] models the surface (the
//! testkit controls below). The real-world leg — wiped vs an OSD
//! superblock vs a foreign signature on the media — is P9-B's to
//! implement against this seam, without redesign.
//!
//! ## Recorded decisions (P9-A)
//!
//! - **The quarantine exit** is the explicit, token-guarded
//!   `ResolveQuarantine` operation. For an unresolved intent it
//!   reconciles the journaled intent (the effect that never landed
//!   stays unlanded: a lend resolves to unlent, a reclaim to
//!   still-lent; a journaled foreign observation is honored into the
//!   foreign-residue quarantine). For foreign residue it records the
//!   operator's inspected-and-accepted resolution and ends the
//!   borrower's tenure (the claim stays). An **unreadable ownership
//!   record has no exit** — the refusal names the manual
//!   remediation; nothing in this slice can reconcile a record it
//!   cannot read.
//! - **The force path's `operator_authorization`** follows the
//!   admin-surface two-token model: the HTTP bearer token
//!   authenticates the caller; the body token is the scoped
//!   operator authorization compared by the provider against its
//!   configured token (a mismatch is `UNSUPPORTED_CLASS_OR_POLICY`,
//!   never logged). A provider configured without a token (the
//!   loopback/test shape) does not compare — production providers
//!   must.
//! - **The frozen surface** (invariant 4) covers the claiming
//!   layer's destructive mutations — claim, release and a second
//!   lend — which are typed refusals while a lend is live or a
//!   quarantine holds. Volume-class operations on a lent device's
//!   pool are outside this slice (readiness D3).

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use volvisor_types::crash::{STORE_DEVICE_OWNERSHIP, StoreCrashHooks, StoreSavePoint};
use volvisor_types::domain::Generation;
use volvisor_types::{
    ApiError, ApiErrorCode, CellId, ClaimedBy, DeviceId, DeviceObservation,
    DeviceOwnershipResponse, DeviceSurfaceObservation, LendDeviceRequest, LendState, OperationId,
    QuarantineReason, QuarantineState, ReclaimDeviceAck, ReclaimDeviceRequest,
    ResolveQuarantineRequest,
};

use crate::grow::{create_owner_only, sibling_tmp_path};

/// Unix epoch seconds now (best effort; tests do not assert exact
/// timestamps).
fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The per-device ownership record: the claim record extended with
/// the lend generations, the quarantine and the pending-intent
/// journal (the durable format of [`DeviceOwnershipStore`]).
///
/// One record carries the whole ownership history the surface needs:
/// the claim (with its generation), the monotonic lend counter, the
/// live lend, the explicit quarantine, and the journaled-but-unapplied
/// intent. Claim, lend and reclaim are generations of this one record
/// — never three independent owners.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DeviceOwnershipRecord {
    /// Whether volvisor's claim holds the device.
    pub claimed: bool,
    /// The claim generation: bumped by every claim and release cycle
    /// (monotonic, never reused).
    pub claim_generation: u64,
    /// The lend generation counter: bumped by every lend (monotonic,
    /// never reused — a reclaim does not reset it).
    pub lend_generation_counter: u64,
    /// The live lend, when one exists.
    pub lend: Option<LendState>,
    /// An explicit quarantine (foreign residue recorded by a
    /// reclaim). Unresolved intents derive their quarantine at read
    /// time instead (see [`effective_quarantine`]).
    pub quarantine: Option<QuarantineState>,
    /// A journaled intent whose effect never landed (the crash
    /// window). Derives a quarantine until reconciled.
    pub pending: Option<PendingIntent>,
    /// The unreadable-record model: a non-`None` marker stands in for
    /// a record that cannot be parsed — the engine treats the whole
    /// record as unreadable (quarantined, no exit), regardless of the
    /// fields it happens to carry.
    pub corrupt: Option<String>,
}

/// A journaled-but-unapplied lend or reclaim intent (the
/// journal-before-mutate half of the record).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PendingIntent {
    /// A lend was journaled; its effect (the live lend sub-record)
    /// never landed. The device is unlent.
    Lend {
        /// The lend operation whose effect is unresolved.
        operation_id: OperationId,
        /// The cell the interrupted lend named.
        borrower_cell_id: CellId,
        /// Unix epoch seconds at the journal append.
        recorded_at: u64,
    },
    /// A reclaim was journaled; its effect never landed. The lend is
    /// still live. The observation the reclaim acted on is carried
    /// verbatim — the reconciliation honors its verdict (a foreign
    /// observation reconciles into the foreign-residue quarantine).
    Reclaim {
        /// The reclaim operation whose effect is unresolved.
        operation_id: OperationId,
        /// What the release/residue observation found, when it ran.
        observation: Option<DeviceSurfaceObservation>,
        /// Unix epoch seconds at the journal append.
        recorded_at: u64,
    },
}

/// The durable ownership-record store: one JSON file per provider
/// (the [`crate::grow`] store discipline — serialize → owner-only
/// tmp write → fsync → rename → directory fsync), consulting the
/// [`STORE_DEVICE_OWNERSHIP`] crash seam at each commit boundary.
pub struct DeviceOwnershipStore {
    path: PathBuf,
    crash: Arc<StoreCrashHooks>,
}

impl DeviceOwnershipStore {
    /// Open the store at `path`. The file is read by [`Self::load`] and
    /// written by [`Self::save`]; construction itself performs no I/O.
    #[must_use]
    pub fn new(path: PathBuf, crash: Arc<StoreCrashHooks>) -> Self {
        Self { path, crash }
    }

    /// The store's file path (named in unreadable-store refusals).
    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Load the records. An absent file is an empty map (a fresh
    /// store); a present-but-unparseable file is a typed error — the
    /// caller fails closed on every operation (the whole-store
    /// unreadable flavor; the per-device flavor is the
    /// [`DeviceOwnershipRecord::corrupt`] marker).
    ///
    /// # Errors
    /// Returns [`ApiError`] when the file exists but cannot be read
    /// or parsed.
    pub fn load(&self) -> Result<BTreeMap<DeviceId, DeviceOwnershipRecord>, ApiError> {
        let internal = |detail: String| ApiError::new(ApiErrorCode::Internal, detail);
        let display = self.path.display();
        match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| {
                internal(format!(
                    "failed to parse the ownership records at {display}: {e}"
                ))
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(e) => Err(internal(format!(
                "failed to read the ownership records at {display}: {e}"
            ))),
        }
    }

    /// Save the records atomically, consulting the crash seam at each
    /// commit boundary (the grow-store discipline). A kill at
    /// `AfterTmpWrite`/`AfterFsyncBeforeRename` leaves the previous
    /// durable state; a kill at `AfterRename` makes the new state
    /// durable — the armed rig chooses the window it proves.
    ///
    /// # Errors
    /// Returns [`ApiError`] on any filesystem failure (the previous
    /// durable state remains live).
    pub fn save(
        &self,
        records: &BTreeMap<DeviceId, DeviceOwnershipRecord>,
    ) -> Result<(), ApiError> {
        save_records_atomic(&self.path, records, &self.crash)
    }
}

/// Persist the records atomically (the `MigrationStore` discipline,
/// shared with the grow-notification store).
fn save_records_atomic(
    path: &std::path::Path,
    records: &BTreeMap<DeviceId, DeviceOwnershipRecord>,
    crash: &StoreCrashHooks,
) -> Result<(), ApiError> {
    let internal = |detail: String| ApiError::new(ApiErrorCode::Internal, detail);
    let tmp_path = sibling_tmp_path(path);
    let tmp_display = tmp_path.display();
    let path_display = path.display();
    let data = serde_json::to_vec_pretty(records)
        .map_err(|e| internal(format!("failed to serialize the ownership records: {e}")))?;

    let mut file = create_owner_only(&tmp_path)
        .map_err(|e| internal(format!("failed to create {tmp_display}: {e}")))?;
    if let Err(e) = file.write_all(&data) {
        drop(std::fs::remove_file(&tmp_path));
        return Err(internal(format!("failed to write {tmp_display}: {e}")));
    }
    crash.consult(STORE_DEVICE_OWNERSHIP, StoreSavePoint::AfterTmpWrite);
    if let Err(e) = file.sync_all() {
        drop(std::fs::remove_file(&tmp_path));
        return Err(internal(format!("failed to fsync {tmp_display}: {e}")));
    }
    drop(file);
    crash.consult(
        STORE_DEVICE_OWNERSHIP,
        StoreSavePoint::AfterFsyncBeforeRename,
    );
    if let Err(e) = std::fs::rename(&tmp_path, path) {
        drop(std::fs::remove_file(&tmp_path));
        return Err(internal(format!(
            "failed to rename {tmp_display} to {path_display}: {e}"
        )));
    }
    crash.consult(STORE_DEVICE_OWNERSHIP, StoreSavePoint::AfterRename);
    // fsync the directory so the rename itself is durable.
    let dir = std::fs::File::open(
        path.parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(std::path::Path::new(".")),
    );
    let result = match dir {
        Ok(dir) => dir
            .sync_all()
            .map_err(|e| internal(format!("failed to fsync the parent of {path_display}: {e}"))),
        Err(e) => Err(internal(format!(
            "failed to open the parent of {path_display}: {e}"
        ))),
    };
    if result.is_err() {
        drop(std::fs::remove_file(&tmp_path));
    }
    result
}

/// The release/residue observation seam (see the module docs for the
/// contract). P9-B's real-world leg implements the real observation
/// (wiped vs OSD superblock vs foreign) against this trait.
pub trait DeviceSurfaceObserver: Send + Sync {
    /// Observe the device's durable surface. Must be safe to call
    /// while the caller holds the ownership-record lock (the LVM
    /// provider's sync-command-under-state-lock precedent).
    ///
    /// # Errors
    /// Returns [`ApiError`] when the surface cannot be observed; the
    /// caller fails closed and writes nothing.
    fn observe(&self, device_id: &DeviceId) -> Result<DeviceSurfaceObservation, ApiError>;
}

/// The privileged device-lending surface (the rook-cell contract's
/// operator operations over claimed devices). Providers serving it
/// attach it through the API layer's `with_lending`; providers
/// without it keep the typed 404.
#[async_trait]
pub trait LendingSurface: Send + Sync {
    /// Lend a claimed device to a borrower cell (journal-before-
    /// mutate; idempotency is the API journal's, keyed by
    /// `operation_id` + request hash).
    ///
    /// # Errors
    /// Returns a typed [`ApiError`] on every refusal: unknown device,
    /// not claimed, quarantined, already lent (`WRITER_ALREADY_ACTIVE`
    /// naming the live lend), stale expected claim generation.
    async fn lend_device(
        &self,
        device_id: &DeviceId,
        request: &LendDeviceRequest,
    ) -> Result<DeviceOwnershipResponse, ApiError>;

    /// Reclaim a lent device: the borrower's release observed, or the
    /// operator-authorized force path with its recorded residue check
    /// (foreign residue quarantines — never a silent reuse).
    ///
    /// # Errors
    /// Returns a typed [`ApiError`] on every refusal: unknown device,
    /// not lent, quarantined, stale expected lend generation, the
    /// borrower still holding (`WRITER_ALREADY_ACTIVE` naming it),
    /// foreign residue (`FOREIGN_DEVICE_STATE`, the device
    /// quarantined), token mismatch.
    async fn reclaim_device(
        &self,
        device_id: &DeviceId,
        request: &ReclaimDeviceRequest,
    ) -> Result<ReclaimDeviceAck, ApiError>;

    /// The explicit quarantine exit (the recorded P9-A decision —
    /// see the module docs).
    ///
    /// # Errors
    /// Returns a typed [`ApiError`] when the device is not
    /// quarantined, the record is unreadable (no exit), or the token
    /// mismatches.
    async fn resolve_device_quarantine(
        &self,
        device_id: &DeviceId,
        request: &ResolveQuarantineRequest,
    ) -> Result<DeviceOwnershipResponse, ApiError>;

    /// The operator-visible ownership answer (read-only).
    ///
    /// # Errors
    /// Returns a typed [`ApiError`] for an unknown device, or when
    /// the whole ownership store cannot be read.
    async fn device_ownership(
        &self,
        device_id: &DeviceId,
    ) -> Result<DeviceOwnershipResponse, ApiError>;
}

/// The effective quarantine of a record: the unreadable marker, an
/// explicit quarantine, or the quarantine derived from an unresolved
/// pending intent (the crash window). Read-time derivation — the
/// loader never rewrites the file.
#[must_use]
pub fn effective_quarantine(record: &DeviceOwnershipRecord) -> Option<QuarantineState> {
    if let Some(details) = &record.corrupt {
        return Some(QuarantineState {
            reason: QuarantineReason::UnreadableRecord {
                details: details.clone(),
            },
            // The corruption's time is unknown; the marker carries no
            // timestamp.
            recorded_at: 0,
        });
    }
    if let Some(quarantine) = &record.quarantine {
        return Some(quarantine.clone());
    }
    match &record.pending {
        Some(PendingIntent::Lend {
            operation_id,
            borrower_cell_id,
            recorded_at,
        }) => Some(QuarantineState {
            reason: QuarantineReason::UnresolvedLendIntent {
                operation_id: operation_id.clone(),
                borrower_cell_id: borrower_cell_id.clone(),
            },
            recorded_at: *recorded_at,
        }),
        Some(PendingIntent::Reclaim {
            operation_id,
            recorded_at,
            ..
        }) => Some(QuarantineState {
            reason: QuarantineReason::UnresolvedReclaimIntent {
                operation_id: operation_id.clone(),
            },
            recorded_at: *recorded_at,
        }),
        None => None,
    }
}

/// The operator-visible view of one record (the read-only
/// `DeviceOwnership` answer).
#[must_use]
pub fn ownership_view(
    device_id: &DeviceId,
    record: &DeviceOwnershipRecord,
) -> DeviceOwnershipResponse {
    if record.corrupt.is_some() {
        // The record cannot be read as a whole: nothing it carries is
        // trusted, so the answer is unknown-claim, not-lent,
        // quarantined-unreadable (fail-closed honesty, not inference).
        return DeviceOwnershipResponse {
            device_id: device_id.clone(),
            claimed_by: ClaimedBy::Unknown,
            claim_generation: Generation(0),
            lend: None,
            quarantined: effective_quarantine(record),
        };
    }
    DeviceOwnershipResponse {
        device_id: device_id.clone(),
        claimed_by: if record.claimed {
            ClaimedBy::Volvisor
        } else {
            ClaimedBy::Unclaimed
        },
        claim_generation: Generation(record.claim_generation),
        lend: record.lend.clone(),
        quarantined: effective_quarantine(record),
    }
}

/// The mutation freeze (invariant 4): a lent or quarantined device
/// refuses the claiming layer's destructive mutations. The refusal
/// names the live lend or the quarantine.
///
/// # Errors
/// Returns a typed [`ApiError`] when the record is unreadable, the
/// device is quarantined, or a lend is live.
pub fn check_mutable(record: &DeviceOwnershipRecord, device_id: &DeviceId) -> Result<(), ApiError> {
    if let Some(quarantine) = effective_quarantine(record) {
        return Err(quarantine_refusal(device_id, &quarantine));
    }
    if let Some(lend) = &record.lend {
        return Err(live_lend_refusal(device_id, lend, "mutated"));
    }
    Ok(())
}

/// The typed `WRITER_ALREADY_ACTIVE` refusal naming the live lend.
fn live_lend_refusal(device_id: &DeviceId, lend: &LendState, verb: &str) -> ApiError {
    ApiError::new(
        ApiErrorCode::WriterAlreadyActive,
        format!(
            "device {device_id} is lent to {} (lend generation {}); reclaim it before the device is {verb}",
            lend.borrower_cell_id, lend.lend_generation.0
        ),
    )
}

/// The typed quarantine refusal naming the reason.
fn quarantine_refusal(device_id: &DeviceId, quarantine: &QuarantineState) -> ApiError {
    ApiError::new(
        ApiErrorCode::InvalidState,
        format!(
            "device {device_id} is quarantined ({:?}); resolve the quarantine first",
            quarantine.reason
        ),
    )
}

/// The typed stale-generation refusal.
fn stale_refusal(expected: u64, found: u64, what: &str) -> ApiError {
    ApiError::new(
        ApiErrorCode::StaleGeneration,
        format!("expected {what} generation {expected}, found {found}"),
    )
}

/// The scoped operator-authorization comparison (the admin-surface
/// two-token model; see the module docs).
fn check_operator_token(configured: Option<&str>, presented: &str) -> Result<(), ApiError> {
    match configured {
        None => Ok(()),
        Some(expected) if presented == expected => Ok(()),
        Some(_) => Err(ApiError::new(
            ApiErrorCode::UnsupportedClassOrPolicy,
            "operator authorization token mismatch",
        )),
    }
}

/// The lend state machine (one record; the caller holds the store
/// lock and provides the save seam — see [`FakeDeviceWorld::lend`]
/// for the composition).
///
/// # Errors
/// Returns a typed [`ApiError`] on every refusal (see
/// [`LendingSurface::lend_device`]) and propagates store failures
/// (the pending intent stays visible on a failed effect save — the
/// device reloads quarantined, fail-closed).
pub fn lend(
    records: &mut BTreeMap<DeviceId, DeviceOwnershipRecord>,
    store: &DeviceOwnershipStore,
    device_id: &DeviceId,
    request: &LendDeviceRequest,
    now: u64,
) -> Result<DeviceOwnershipResponse, ApiError> {
    request.validate()?;
    // Checks (immutable; a refusal writes nothing).
    {
        let record = records
            .get(device_id)
            .ok_or_else(|| ApiError::not_found(format!("unknown device {device_id}")))?;
        if record.corrupt.is_some() {
            return Err(unreadable_refusal(device_id, record));
        }
        if !record.claimed {
            return Err(ApiError::new(
                ApiErrorCode::InvalidState,
                format!("device {device_id} is not claimed; a lend requires volvisor's claim"),
            ));
        }
        if let Some(quarantine) = effective_quarantine(record) {
            return Err(quarantine_refusal(device_id, &quarantine));
        }
        if let Some(lend) = &record.lend {
            return Err(live_lend_refusal(device_id, lend, "lent again"));
        }
        if request.expected_claim_generation.0 != record.claim_generation {
            return Err(stale_refusal(
                request.expected_claim_generation.0,
                record.claim_generation,
                "claim",
            ));
        }
    }

    // 1. Journal the intent (durable before the effect): a crash from
    //    here on leaves the pending intent visible, which derives the
    //    unlent-but-quarantined state.
    {
        let record = record_for(records, device_id)?;
        record.pending = Some(PendingIntent::Lend {
            operation_id: request.operation_id.clone(),
            borrower_cell_id: request.borrower_cell_id.clone(),
            recorded_at: now,
        });
    }
    store.save(records)?;

    // 2. Apply (the effect): one save clears the intent and lands the
    //    lend atomically — the record never shows a landed lend with
    //    a visible intent, and never a vanished intent without the
    //    lend.
    let view = {
        let record = record_for(records, device_id)?;
        record.lend_generation_counter += 1;
        record.lend = Some(LendState {
            borrower_cell_id: request.borrower_cell_id.clone(),
            lend_generation: Generation(record.lend_generation_counter),
            recorded_at: now,
            // The POC's evidence discipline (P9-B) owns real references;
            // this slice records none rather than inventing a scheme.
            evidence_ref: None,
        });
        record.pending = None;
        ownership_view(device_id, record)
    };
    store.save(records)?;

    Ok(view)
}

/// The reclaim state machine (the release path and the force path —
/// see [`LendingSurface::reclaim_device`]).
///
/// # Errors
/// Returns a typed [`ApiError`] on every refusal and propagates
/// observation/store failures (nothing is written when the
/// observation fails; a failed effect save leaves the pending
/// intent visible — the device reloads quarantined).
pub fn reclaim(
    records: &mut BTreeMap<DeviceId, DeviceOwnershipRecord>,
    store: &DeviceOwnershipStore,
    observer: &dyn DeviceSurfaceObserver,
    device_id: &DeviceId,
    request: &ReclaimDeviceRequest,
    operator_token: Option<&str>,
    now: u64,
) -> Result<ReclaimDeviceAck, ApiError> {
    request.validate()?;
    // Checks (immutable; a refusal writes nothing).
    {
        let record = records
            .get(device_id)
            .ok_or_else(|| ApiError::not_found(format!("unknown device {device_id}")))?;
        if record.corrupt.is_some() {
            return Err(unreadable_refusal(device_id, record));
        }
        if !record.claimed {
            return Err(ApiError::new(
                ApiErrorCode::InvalidState,
                format!("device {device_id} is not claimed"),
            ));
        }
        if let Some(quarantine) = effective_quarantine(record) {
            return Err(quarantine_refusal(device_id, &quarantine));
        }
        let lend = record.lend.clone().ok_or_else(|| {
            ApiError::new(
                ApiErrorCode::InvalidState,
                format!("device {device_id} is not lent; there is nothing to reclaim"),
            )
        })?;
        if request.expected_lend_generation.0 != lend.lend_generation.0 {
            return Err(stale_refusal(
                request.expected_lend_generation.0,
                lend.lend_generation.0,
                "lend",
            ));
        }
    }
    if let Some(force) = &request.force {
        check_operator_token(operator_token, &force.operator_authorization)?;
    }

    // The observation (the seam): the plain path requires the
    // borrower's release; the force path records whatever the check
    // finds. An observation failure refuses before anything is
    // written (fail-closed, no state claims).
    let observation = observer.observe(device_id)?;
    if request.force.is_none() {
        if let DeviceSurfaceObservation::BorrowerSignature { borrower_cell_id } = &observation {
            return Err(ApiError::new(
                ApiErrorCode::WriterAlreadyActive,
                format!(
                    "the borrower {borrower_cell_id} still holds device {device_id}; \
                     reclaim requires its release or an operator-authorized force"
                ),
            ));
        }
    }

    // 1. Journal the intent, carrying the observation verbatim (the
    //    reconciliation honors its verdict).
    {
        let record = record_for(records, device_id)?;
        record.pending = Some(PendingIntent::Reclaim {
            operation_id: request.operation_id.clone(),
            observation: Some(observation.clone()),
            recorded_at: now,
        });
    }
    store.save(records)?;

    // 2. Apply: foreign residue quarantines (never a silent reuse);
    //    anything else completes the reclaim.
    if let DeviceSurfaceObservation::Foreign { details } = &observation {
        {
            let record = record_for(records, device_id)?;
            record.pending = None;
            record.quarantine = Some(QuarantineState {
                reason: QuarantineReason::ForeignResidue {
                    details: details.clone(),
                },
                recorded_at: now,
            });
        }
        store.save(records)?;
        let forced = request.force.is_some();
        return Err(ApiError::new(
            ApiErrorCode::ForeignDeviceState,
            format!(
                "device {device_id} shows foreign residue ({details}); the device is \
                 quarantined, never silently reused ({} path)",
                if forced { "force reclaim" } else { "reclaim" }
            ),
        ));
    }
    {
        let record = record_for(records, device_id)?;
        record.pending = None;
        record.lend = None;
    }
    store.save(records)?;

    Ok(ReclaimDeviceAck {
        reclaimed: device_id.clone(),
        observation: DeviceObservation {
            observed: observation,
            recorded_at: now,
        },
    })
}

/// The quarantine-exit state machine (the recorded P9-A decision —
/// see the module docs).
///
/// # Errors
/// Returns a typed [`ApiError`] when the device is not quarantined,
/// the record is unreadable (no exit), or the token mismatches.
pub fn resolve_quarantine(
    records: &mut BTreeMap<DeviceId, DeviceOwnershipRecord>,
    store: &DeviceOwnershipStore,
    device_id: &DeviceId,
    request: &ResolveQuarantineRequest,
    operator_token: Option<&str>,
) -> Result<DeviceOwnershipResponse, ApiError> {
    request.validate()?;
    check_operator_token(operator_token, &request.operator_authorization)?;

    // Determine and apply in one scope, then save once: an unresolved
    // intent reconciles by what the durable record proves (the effect
    // never landed, so the pre-intent state stands — a lend resolves
    // to unlent, a reclaim to still-lent; a journaled foreign
    // observation is honored into the foreign-residue quarantine,
    // because ignoring it would risk reusing foreign residue). An
    // explicit foreign-residue quarantine resolves by the operator's
    // inspected-and-accepted judgment: the borrower's tenure ends,
    // the claim stays.
    let view = {
        let record = record_for(records, device_id)?;
        if record.corrupt.is_some() {
            return Err(unreadable_no_exit(device_id, record, store));
        }
        if let Some(pending) = record.pending.take() {
            match pending {
                PendingIntent::Lend { .. } => {
                    // Unlent (the lend never landed); the quarantine
                    // derived from the intent lifts with it.
                }
                PendingIntent::Reclaim { observation, .. } => {
                    if let Some(DeviceSurfaceObservation::Foreign { details }) = observation {
                        record.quarantine = Some(QuarantineState {
                            reason: QuarantineReason::ForeignResidue { details },
                            recorded_at: now_seconds(),
                        });
                    }
                    // Any other observation: still lent (the reclaim
                    // never landed), quarantine lifted.
                }
            }
            ownership_view(device_id, record)
        } else {
            match record.quarantine.take() {
                None => {
                    return Err(ApiError::new(
                        ApiErrorCode::InvalidState,
                        format!("device {device_id} is not quarantined"),
                    ));
                }
                Some(quarantine) => match quarantine.reason {
                    QuarantineReason::ForeignResidue { .. } => {
                        record.lend = None;
                        ownership_view(device_id, record)
                    }
                    // Derived reasons are never stored explicitly, and an
                    // unreadable record was refused above; refuse rather
                    // than guess what these would mean.
                    reason => {
                        record.quarantine = Some(QuarantineState {
                            reason,
                            recorded_at: quarantine.recorded_at,
                        });
                        return Err(ApiError::new(
                            ApiErrorCode::InvalidState,
                            format!(
                                "device {device_id} carries a quarantine this surface cannot \
                                 resolve; repair the ownership record manually"
                            ),
                        ));
                    }
                },
            }
        }
    };
    store.save(records)?;
    Ok(view)
}

/// The typed refusal for an unreadable per-device record.
fn unreadable_refusal(device_id: &DeviceId, record: &DeviceOwnershipRecord) -> ApiError {
    ApiError::new(
        ApiErrorCode::InvalidState,
        format!(
            "device {device_id}'s ownership record is unreadable ({}); it is quarantined \
             and admits no operation",
            record.corrupt.as_deref().unwrap_or("unreadable")
        ),
    )
}

/// The typed no-exit refusal for an unreadable per-device record.
fn unreadable_no_exit(
    device_id: &DeviceId,
    record: &DeviceOwnershipRecord,
    store: &DeviceOwnershipStore,
) -> ApiError {
    ApiError::new(
        ApiErrorCode::InvalidState,
        format!(
            "device {device_id}'s ownership record is unreadable ({}); there is no \
             automated exit — inspect and repair {} manually",
            record.corrupt.as_deref().unwrap_or("unreadable"),
            store.path().display()
        ),
    )
}

/// The record for `device_id`, or the typed 404.
fn record_for<'a>(
    records: &'a mut BTreeMap<DeviceId, DeviceOwnershipRecord>,
    device_id: &DeviceId,
) -> Result<&'a mut DeviceOwnershipRecord, ApiError> {
    records
        .get_mut(device_id)
        .ok_or_else(|| ApiError::not_found(format!("unknown device {device_id}")))
}

/// The modeled device surface of the Tier S world: what the borrower
/// leaves observable, driven by the testkit controls below.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ModeledDeviceSurface {
    /// No borrower signature observable (released / wiped).
    #[default]
    Clean,
    /// The named borrower's signature.
    Borrower(CellId),
    /// A foreign signature (the recorded details).
    Foreign(String),
    /// The observation itself fails (the seam's error path).
    Unreadable,
}

/// The fake's durable device world: the ownership-record store, the
/// modeled device surfaces (the observation seam's Tier S
/// implementation) and the optional operator-authorization token.
///
/// Constructed through [`crate::FakeProvider::with_durable_devices`];
/// the world survives daemon restarts by construction (the state
/// file), which is what makes the lend/reclaim crash windows
/// provable at the daemon level.
pub struct FakeDeviceWorld {
    store: DeviceOwnershipStore,
    operator_token: Option<String>,
    surfaces: Mutex<BTreeMap<DeviceId, ModeledDeviceSurface>>,
    records: Mutex<Result<BTreeMap<DeviceId, DeviceOwnershipRecord>, ApiError>>,
}

impl std::fmt::Debug for FakeDeviceWorld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeDeviceWorld")
            .field("path", &self.store.path)
            .field("operator_token_configured", &self.operator_token.is_some())
            .finish_non_exhaustive()
    }
}

impl FakeDeviceWorld {
    /// Open the world over `path` (loading the records; an
    /// unparseable file fails every operation closed — the
    /// whole-store unreadable flavor).
    ///
    /// The fake's single device is seeded into a fresh map: a device
    /// with no record is not a device this world serves, and the
    /// entry (unclaimed, generation 0) is the honest starting state.
    #[must_use]
    pub fn open(
        path: PathBuf,
        crash: Arc<StoreCrashHooks>,
        operator_token: Option<String>,
    ) -> Self {
        let store = DeviceOwnershipStore::new(path, crash);
        let records = store
            .load()
            .and_then(|mut records| {
                let device = DeviceId::new(crate::admin::FAKE_DEVICE_ID)?;
                records.entry(device).or_default();
                Ok(records)
            })
            .map_err(|e| {
                ApiError::new(
                    ApiErrorCode::ForeignDeviceState,
                    format!(
                        "the device-ownership store at {} cannot be read; treat every device \
                         as quarantined: {e}",
                        store.path().display()
                    ),
                )
            });
        Self {
            store,
            operator_token,
            surfaces: Mutex::new(BTreeMap::new()),
            records: Mutex::new(records),
        }
    }

    /// The store-save crash seam (the armed table a rig aims and the
    /// kill switch fires into; inert unless armed).
    #[must_use]
    pub fn store_crash_hooks(&self) -> &Arc<StoreCrashHooks> {
        &self.store.crash
    }

    /// The configured operator-authorization token, when present.
    #[must_use]
    pub fn operator_token(&self) -> Option<&str> {
        self.operator_token.as_deref()
    }

    /// Run `f` under the records lock with the store; an unreadable
    /// store refuses closed before `f` runs.
    fn with_records<T>(
        &self,
        f: impl FnOnce(
            &mut BTreeMap<DeviceId, DeviceOwnershipRecord>,
            &DeviceOwnershipStore,
        ) -> Result<T, ApiError>,
    ) -> Result<T, ApiError> {
        let mut guard = self.records.lock().map_err(|_| {
            ApiError::new(
                ApiErrorCode::Internal,
                "the device-ownership records lock is poisoned",
            )
        })?;
        match &mut *guard {
            Err(unreadable) => Err(unreadable.clone()),
            Ok(records) => f(records, &self.store),
        }
    }

    /// Lend through the engine (the composition the API layer
    /// calls).
    ///
    /// # Errors
    /// Propagates the engine's typed refusals.
    pub fn lend(
        &self,
        device_id: &DeviceId,
        request: &LendDeviceRequest,
    ) -> Result<DeviceOwnershipResponse, ApiError> {
        self.with_records(|records, store| lend(records, store, device_id, request, now_seconds()))
    }

    /// Reclaim through the engine, observing the modeled surface.
    ///
    /// # Errors
    /// Propagates the engine's typed refusals.
    pub fn reclaim(
        &self,
        device_id: &DeviceId,
        request: &ReclaimDeviceRequest,
    ) -> Result<ReclaimDeviceAck, ApiError> {
        self.with_records(|records, store| {
            reclaim(
                records,
                store,
                self,
                device_id,
                request,
                self.operator_token.as_deref(),
                now_seconds(),
            )
        })
    }

    /// Resolve a quarantine through the engine.
    ///
    /// # Errors
    /// Propagates the engine's typed refusals.
    pub fn resolve_quarantine(
        &self,
        device_id: &DeviceId,
        request: &ResolveQuarantineRequest,
    ) -> Result<DeviceOwnershipResponse, ApiError> {
        self.with_records(|records, store| {
            resolve_quarantine(
                records,
                store,
                device_id,
                request,
                self.operator_token.as_deref(),
            )
        })
    }

    /// The operator-visible ownership answer.
    ///
    /// # Errors
    /// Propagates the typed unknown-device and unreadable-store
    /// refusals.
    pub fn ownership(&self, device_id: &DeviceId) -> Result<DeviceOwnershipResponse, ApiError> {
        self.with_records(|records, _| {
            let record = record_for(records, device_id)?;
            Ok(ownership_view(device_id, record))
        })
    }

    /// The durable claim (the admin surface's durable mode): freeze
    /// checks, then claim at the next generation.
    ///
    /// # Errors
    /// Returns a typed [`ApiError`] when the device is quarantined,
    /// lent, already claimed, or the store is unreadable.
    pub fn claim(&self, device_id: &DeviceId) -> Result<Generation, ApiError> {
        self.with_records(|records, store| {
            let generation = {
                let record = record_for(records, device_id)?;
                check_mutable(record, device_id)?;
                if record.claimed {
                    return Err(ApiError::new(
                        ApiErrorCode::InvalidState,
                        format!("device {device_id} is already claimed"),
                    ));
                }
                record.claimed = true;
                record.claim_generation += 1;
                Generation(record.claim_generation)
            };
            store.save(records)?;
            Ok(generation)
        })
    }

    /// The durable release (the admin surface's durable mode): freeze
    /// checks, then release at the next generation.
    ///
    /// # Errors
    /// Returns a typed [`ApiError`] when the device is quarantined,
    /// lent, not claimed, or the store is unreadable.
    pub fn release(&self, device_id: &DeviceId) -> Result<(), ApiError> {
        self.with_records(|records, store| {
            {
                let record = record_for(records, device_id)?;
                check_mutable(record, device_id)?;
                if !record.claimed {
                    return Err(ApiError::new(
                        ApiErrorCode::InvalidState,
                        format!("device {device_id} is not claimed"),
                    ));
                }
                record.claimed = false;
                record.claim_generation += 1;
            }
            store.save(records)
        })
    }

    /// The claim state for discovery (the durable mode's view). An
    /// unreadable record refuses: the inventory cannot answer for a
    /// record it does not trust.
    ///
    /// # Errors
    /// Returns a typed [`ApiError`] for an unknown device, an
    /// unreadable record, or an unreadable store.
    pub fn claim_state(&self, device_id: &DeviceId) -> Result<(bool, Generation), ApiError> {
        self.with_records(|records, _| {
            let record = record_for(records, device_id)?;
            if record.corrupt.is_some() {
                return Err(unreadable_refusal(device_id, record));
            }
            Ok((record.claimed, Generation(record.claim_generation)))
        })
    }

    /// Testkit: the borrower takes the device (its signature appears
    /// on the modeled surface).
    ///
    /// # Errors
    /// Returns a typed [`ApiError`] when the model lock is poisoned.
    pub fn model_borrower_takes(
        &self,
        device_id: &DeviceId,
        borrower: &CellId,
    ) -> Result<(), ApiError> {
        self.set_surface(device_id, ModeledDeviceSurface::Borrower(borrower.clone()))
    }

    /// Testkit: the borrower releases the device (cell teardown —
    /// its signature leaves the modeled surface).
    ///
    /// # Errors
    /// Returns a typed [`ApiError`] when the model lock is poisoned.
    pub fn model_borrower_release(&self, device_id: &DeviceId) -> Result<(), ApiError> {
        self.set_surface(device_id, ModeledDeviceSurface::Clean)
    }

    /// Testkit: a foreign signature appears on the modeled surface.
    ///
    /// # Errors
    /// Returns a typed [`ApiError`] when the model lock is poisoned.
    pub fn model_foreign_residue(
        &self,
        device_id: &DeviceId,
        details: &str,
    ) -> Result<(), ApiError> {
        self.set_surface(device_id, ModeledDeviceSurface::Foreign(details.to_owned()))
    }

    /// Testkit: the observation itself fails (the seam's error
    /// path).
    ///
    /// # Errors
    /// Returns a typed [`ApiError`] when the model lock is poisoned.
    pub fn model_surface_unreadable(&self, device_id: &DeviceId) -> Result<(), ApiError> {
        self.set_surface(device_id, ModeledDeviceSurface::Unreadable)
    }

    /// Testkit: mark the device's ownership record unreadable. The
    /// marker stands in for a record that cannot be parsed: the
    /// engine treats the whole record as unreadable (quarantined, no
    /// exit), regardless of the fields it carries. Persisted, so a
    /// restart sees the same state.
    ///
    /// # Errors
    /// Returns a typed [`ApiError`] when the store cannot be written.
    pub fn inject_unreadable_record(&self, device_id: &DeviceId) -> Result<(), ApiError> {
        self.with_records(|records, store| {
            {
                let record = record_for(records, device_id)?;
                record.corrupt = Some(
                    "injected unreadable record (testkit model of an unparseable record)"
                        .to_owned(),
                );
            }
            store.save(records)
        })
    }

    fn set_surface(
        &self,
        device_id: &DeviceId,
        surface: ModeledDeviceSurface,
    ) -> Result<(), ApiError> {
        let mut surfaces = self.surfaces.lock().map_err(|_| {
            ApiError::new(
                ApiErrorCode::Internal,
                "the device-surface model lock is poisoned",
            )
        })?;
        surfaces.insert(device_id.clone(), surface);
        Ok(())
    }
}

impl DeviceSurfaceObserver for FakeDeviceWorld {
    fn observe(&self, device_id: &DeviceId) -> Result<DeviceSurfaceObservation, ApiError> {
        let surfaces = self.surfaces.lock().map_err(|_| {
            ApiError::new(
                ApiErrorCode::Internal,
                "the device-surface model lock is poisoned",
            )
        })?;
        match surfaces
            .get(device_id)
            .unwrap_or(&ModeledDeviceSurface::Clean)
        {
            ModeledDeviceSurface::Clean => Ok(DeviceSurfaceObservation::Released),
            ModeledDeviceSurface::Borrower(borrower) => {
                Ok(DeviceSurfaceObservation::BorrowerSignature {
                    borrower_cell_id: borrower.clone(),
                })
            }
            ModeledDeviceSurface::Foreign(details) => Ok(DeviceSurfaceObservation::Foreign {
                details: details.clone(),
            }),
            ModeledDeviceSurface::Unreadable => Err(ApiError::new(
                ApiErrorCode::Internal,
                format!("the device surface of {device_id} cannot be observed"),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use volvisor_types::crash::StoreCrashHooks;

    fn device() -> DeviceId {
        DeviceId::new("dev-fake-1").expect("id")
    }

    fn borrower() -> CellId {
        CellId::new("cell-a").expect("id")
    }

    fn lend_req(operation_id: &str, expected_claim_generation: u64) -> LendDeviceRequest {
        serde_json::from_value(serde_json::json!({
            "api_version": "volvisor.volume.v2",
            "operation_id": operation_id,
            "borrower_cell_id": "cell-a",
            "expected_claim_generation": expected_claim_generation,
        }))
        .expect("valid")
    }

    fn reclaim_req(expected_lend_generation: u64, force: Option<&str>) -> ReclaimDeviceRequest {
        let force_json = force.map(|token| {
            serde_json::json!({
                "operator_authorization": token,
                "residue_check": "inspect_and_record"
            })
        });
        serde_json::from_value(serde_json::json!({
            "api_version": "volvisor.volume.v2",
            "operation_id": "op-reclaim",
            "expected_lend_generation": expected_lend_generation,
            "force": force_json,
        }))
        .expect("valid")
    }

    fn store() -> (DeviceOwnershipStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = DeviceOwnershipStore::new(
            dir.path().join("ownership.json"),
            Arc::new(StoreCrashHooks::new()),
        );
        (store, dir)
    }

    fn claimed_records() -> BTreeMap<DeviceId, DeviceOwnershipRecord> {
        let mut records = BTreeMap::new();
        records.insert(
            device(),
            DeviceOwnershipRecord {
                claimed: true,
                claim_generation: 1,
                ..DeviceOwnershipRecord::default()
            },
        );
        records
    }

    struct CleanObserver;
    impl DeviceSurfaceObserver for CleanObserver {
        fn observe(&self, _: &DeviceId) -> Result<DeviceSurfaceObservation, ApiError> {
            Ok(DeviceSurfaceObservation::Released)
        }
    }

    /// Observes the named borrower still holding the surface.
    struct HeldObserver;
    impl DeviceSurfaceObserver for HeldObserver {
        fn observe(&self, _: &DeviceId) -> Result<DeviceSurfaceObservation, ApiError> {
            Ok(DeviceSurfaceObservation::BorrowerSignature {
                borrower_cell_id: CellId::new("cell-a").expect("id"),
            })
        }
    }

    /// Observes a foreign signature on the surface.
    struct ForeignObserver;
    impl DeviceSurfaceObserver for ForeignObserver {
        fn observe(&self, _: &DeviceId) -> Result<DeviceSurfaceObservation, ApiError> {
            Ok(DeviceSurfaceObservation::Foreign {
                details: "foreign superblock".to_owned(),
            })
        }
    }

    /// The observation itself fails.
    struct FailingObserver;
    impl DeviceSurfaceObserver for FailingObserver {
        fn observe(&self, _: &DeviceId) -> Result<DeviceSurfaceObservation, ApiError> {
            Err(ApiError::new(
                ApiErrorCode::Internal,
                "the surface cannot be observed",
            ))
        }
    }

    #[test]
    fn lend_happy_path_bumps_the_lend_generation() {
        let (store, _dir) = store();
        let mut records = claimed_records();
        let view = lend(&mut records, &store, &device(), &lend_req("op-1", 1), 100).expect("lend");
        assert_eq!(view.claimed_by, ClaimedBy::Volvisor);
        assert_eq!(view.claim_generation, Generation(1));
        let lend_state = view.lend.expect("lent");
        assert_eq!(lend_state.borrower_cell_id, borrower());
        assert_eq!(lend_state.lend_generation, Generation(1));
        assert_eq!(lend_state.recorded_at, 100);
        assert!(view.quarantined.is_none());

        // A second lend is refused naming the live lend.
        let err = lend(&mut records, &store, &device(), &lend_req("op-2", 1), 101)
            .expect_err("double lend");
        assert_eq!(err.code, ApiErrorCode::WriterAlreadyActive);
        assert!(err.detail.contains("cell-a"));
        assert!(err.detail.contains("lend generation 1"));
    }

    #[test]
    fn lend_refusals_are_typed() {
        let (store, _dir) = store();
        // Stale claim generation.
        let mut records = claimed_records();
        let err = lend(&mut records, &store, &device(), &lend_req("op-1", 9), 100)
            .expect_err("stale generation");
        assert_eq!(err.code, ApiErrorCode::StaleGeneration);

        // Not claimed.
        let mut records = claimed_records();
        records.get_mut(&device()).expect("record").claimed = false;
        let err = lend(&mut records, &store, &device(), &lend_req("op-1", 1), 100)
            .expect_err("unclaimed");
        assert_eq!(err.code, ApiErrorCode::InvalidState);

        // Quarantined (an unresolved pending intent derives it).
        let mut records = claimed_records();
        records.get_mut(&device()).expect("record").pending = Some(PendingIntent::Lend {
            operation_id: OperationId::new("op-crashed").expect("id"),
            borrower_cell_id: borrower(),
            recorded_at: 99,
        });
        let err = lend(&mut records, &store, &device(), &lend_req("op-1", 1), 100)
            .expect_err("quarantined");
        assert_eq!(err.code, ApiErrorCode::InvalidState);
        assert!(err.detail.contains("quarantined"));

        // Unreadable record: refused, surfaced as unknown-claim.
        let mut records = claimed_records();
        records.get_mut(&device()).expect("record").corrupt = Some("injected".to_owned());
        let err = lend(&mut records, &store, &device(), &lend_req("op-1", 1), 100)
            .expect_err("unreadable");
        assert_eq!(err.code, ApiErrorCode::InvalidState);
        let view = ownership_view(&device(), records.get(&device()).expect("record"));
        assert_eq!(view.claimed_by, ClaimedBy::Unknown);
        assert!(view.lend.is_none());
        assert!(matches!(
            view.quarantined.expect("quarantined").reason,
            QuarantineReason::UnreadableRecord { .. }
        ));

        // Unknown device.
        let mut records = BTreeMap::new();
        let unknown = DeviceId::new("dev-other").expect("id");
        let err =
            lend(&mut records, &store, &unknown, &lend_req("op-1", 1), 100).expect_err("unknown");
        assert_eq!(err.code, ApiErrorCode::NotFound);
    }

    #[test]
    fn reclaim_release_path_and_force_paths() {
        let (store, _dir) = store();
        let mut records = claimed_records();
        lend(&mut records, &store, &device(), &lend_req("op-1", 1), 100).expect("lend");

        // The borrower still holds: refused naming it.
        let err = reclaim(
            &mut records,
            &store,
            &HeldObserver,
            &device(),
            &reclaim_req(1, None),
            None,
            101,
        )
        .expect_err("borrower holds");
        assert_eq!(err.code, ApiErrorCode::WriterAlreadyActive);
        assert!(err.detail.contains("cell-a"));

        // Released: the reclaim completes and records the observation.
        let ack = reclaim(
            &mut records,
            &store,
            &CleanObserver,
            &device(),
            &reclaim_req(1, None),
            None,
            102,
        )
        .expect("reclaim");
        assert_eq!(ack.reclaimed, device());
        assert_eq!(ack.observation.observed, DeviceSurfaceObservation::Released);
        assert!(records.get(&device()).expect("record").lend.is_none());

        // The next lend gets a fresh (never reused) generation.
        lend(&mut records, &store, &device(), &lend_req("op-2", 1), 103).expect("lend");
        assert_eq!(
            records
                .get(&device())
                .expect("record")
                .lend
                .as_ref()
                .expect("lend")
                .lend_generation,
            Generation(2)
        );

        // Stale lend generation.
        let err = reclaim(
            &mut records,
            &store,
            &CleanObserver,
            &device(),
            &reclaim_req(1, None),
            None,
            104,
        )
        .expect_err("stale");
        assert_eq!(err.code, ApiErrorCode::StaleGeneration);

        // Force with the wrong token: refused, nothing written.
        let err = reclaim(
            &mut records,
            &store,
            &CleanObserver,
            &device(),
            &reclaim_req(2, Some("op-token")),
            Some("the-real-token"),
            105,
        )
        .expect_err("token mismatch");
        assert_eq!(err.code, ApiErrorCode::UnsupportedClassOrPolicy);
        assert!(records.get(&device()).expect("record").lend.is_some());

        // Force over an unclean exit (the borrower's own signature):
        // completes and records the residue check.
        let ack = reclaim(
            &mut records,
            &store,
            &HeldObserver,
            &device(),
            &reclaim_req(2, Some("the-real-token")),
            Some("the-real-token"),
            106,
        )
        .expect("force reclaim");
        assert_eq!(
            ack.observation.observed,
            DeviceSurfaceObservation::BorrowerSignature {
                borrower_cell_id: borrower()
            }
        );
        assert!(records.get(&device()).expect("record").lend.is_none());
    }

    #[test]
    fn foreign_residue_quarantines() {
        let (store, _dir) = store();
        let mut records = claimed_records();
        lend(&mut records, &store, &device(), &lend_req("op-1", 1), 100).expect("lend");

        // The plain path quarantines (fail-closed: the surface
        // disagrees with the record).
        let err = reclaim(
            &mut records,
            &store,
            &ForeignObserver,
            &device(),
            &reclaim_req(1, None),
            None,
            101,
        )
        .expect_err("foreign");
        assert_eq!(err.code, ApiErrorCode::ForeignDeviceState);
        let record = records.get(&device()).expect("record");
        let quarantine = record.quarantine.as_ref().expect("quarantined");
        assert!(matches!(
            quarantine.reason,
            QuarantineReason::ForeignResidue { .. }
        ));
        // The reclaim did not complete: the lend is still live.
        assert!(record.lend.is_some());

        // The operator's explicit resolution ends the tenure.
        let resolve_req: ResolveQuarantineRequest = serde_json::from_value(serde_json::json!({
            "api_version": "volvisor.volume.v2",
            "operation_id": "op-resolve",
            "operator_authorization": "op-token"
        }))
        .expect("valid");
        let view = resolve_quarantine(&mut records, &store, &device(), &resolve_req, None)
            .expect("resolve");
        assert!(view.quarantined.is_none());
        assert!(view.lend.is_none());
        assert_eq!(view.claimed_by, ClaimedBy::Volvisor);
    }

    #[test]
    fn pending_intents_derive_quarantines_and_reconcile() {
        let (store, _dir) = store();

        // A crashed lend: pending visible, unlent, quarantined.
        let mut records = claimed_records();
        records.get_mut(&device()).expect("record").pending = Some(PendingIntent::Lend {
            operation_id: OperationId::new("op-crashed").expect("id"),
            borrower_cell_id: borrower(),
            recorded_at: 99,
        });
        let view = ownership_view(&device(), records.get(&device()).expect("record"));
        assert!(view.lend.is_none());
        let quarantine = view.quarantined.expect("quarantined");
        assert!(matches!(
            quarantine.reason,
            QuarantineReason::UnresolvedLendIntent { .. }
        ));
        assert_eq!(quarantine.recorded_at, 99);
        // The freeze holds for the claiming layer.
        assert!(check_mutable(records.get(&device()).expect("record"), &device()).is_err());

        // Reconciliation: the lend never landed, so the device is
        // unlent and operable again.
        let resolve_req: ResolveQuarantineRequest = serde_json::from_value(serde_json::json!({
            "api_version": "volvisor.volume.v2",
            "operation_id": "op-resolve",
            "operator_authorization": "op-token"
        }))
        .expect("valid");
        let view = resolve_quarantine(&mut records, &store, &device(), &resolve_req, None)
            .expect("resolve");
        assert!(view.quarantined.is_none());
        assert!(view.lend.is_none());
        lend(
            &mut records,
            &store,
            &device(),
            &lend_req("op-fresh", 1),
            200,
        )
        .expect("fresh lend");

        // A crashed reclaim: pending visible, still lent, quarantined;
        // reconciliation keeps the lend live.
        records.get_mut(&device()).expect("record").pending = Some(PendingIntent::Reclaim {
            operation_id: OperationId::new("op-crashed-reclaim").expect("id"),
            observation: Some(DeviceSurfaceObservation::Released),
            recorded_at: 201,
        });
        let view = ownership_view(&device(), records.get(&device()).expect("record"));
        assert!(matches!(
            view.quarantined.expect("quarantined").reason,
            QuarantineReason::UnresolvedReclaimIntent { .. }
        ));
        let view = resolve_quarantine(&mut records, &store, &device(), &resolve_req, None)
            .expect("resolve");
        assert!(view.quarantined.is_none());
        assert!(view.lend.is_some());

        // A crashed force reclaim whose journaled observation was
        // foreign: the reconciliation honors the verdict.
        records.get_mut(&device()).expect("record").pending = Some(PendingIntent::Reclaim {
            operation_id: OperationId::new("op-crashed-foreign").expect("id"),
            observation: Some(DeviceSurfaceObservation::Foreign {
                details: "foreign superblock".to_owned(),
            }),
            recorded_at: 202,
        });
        let view = resolve_quarantine(&mut records, &store, &device(), &resolve_req, None)
            .expect("resolve");
        let quarantine = view.quarantined.expect("still quarantined");
        assert!(matches!(
            quarantine.reason,
            QuarantineReason::ForeignResidue { .. }
        ));

        // Resolving a device that is not quarantined is a typed
        // refusal; an unreadable record has no exit at all.
        let view = resolve_quarantine(&mut records, &store, &device(), &resolve_req, None)
            .expect("resolve");
        assert!(view.quarantined.is_none());
        let err = resolve_quarantine(&mut records, &store, &device(), &resolve_req, None)
            .expect_err("not quarantined");
        assert_eq!(err.code, ApiErrorCode::InvalidState);

        records.get_mut(&device()).expect("record").corrupt = Some("injected".to_owned());
        let err = resolve_quarantine(&mut records, &store, &device(), &resolve_req, None)
            .expect_err("no exit");
        assert_eq!(err.code, ApiErrorCode::InvalidState);
        assert!(err.detail.contains("no automated exit"));
    }

    #[test]
    fn observation_failure_writes_nothing() {
        let (store, _dir) = store();
        let mut records = claimed_records();
        lend(&mut records, &store, &device(), &lend_req("op-1", 1), 100).expect("lend");

        let err = reclaim(
            &mut records,
            &store,
            &FailingObserver,
            &device(),
            &reclaim_req(1, None),
            None,
            101,
        )
        .expect_err("observation fails");
        assert_eq!(err.code, ApiErrorCode::Internal);

        // Nothing was written: no pending intent, the lend intact.
        let record = records.get(&device()).expect("record");
        assert!(record.pending.is_none());
        assert!(record.lend.is_some());
    }

    #[test]
    fn the_store_round_trips_and_fails_closed_on_garbage() {
        let (store, dir) = store();
        let mut records = claimed_records();
        lend(&mut records, &store, &device(), &lend_req("op-1", 1), 100).expect("lend");
        store.save(&records).expect("save");

        let loaded = store.load().expect("load");
        assert_eq!(loaded, records);

        // A garbage file is a typed unreadable-store error.
        std::fs::write(dir.path().join("ownership.json"), b"not json").expect("write");
        let err = store.load().expect_err("garbage");
        assert_eq!(err.code, ApiErrorCode::Internal);
        assert!(err.detail.contains("cannot be read") || err.detail.contains("failed to"));
    }
}
