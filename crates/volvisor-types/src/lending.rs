//! Device-lending request and response shapes (the rook-cell
//! contract's "Device lending and sharing" surface, P9-A).
//!
//! Lending shares a volvisor-claimed device with a named borrower (a
//! cell) **without surrendering the claim**: the lend is a generation
//! of the device's one ownership record, never a second owner. The
//! operations are privileged mutations and journal through the same
//! pipeline as claim/release (intent durable before the effect,
//! idempotent by `operation_id` + immutable request hash).
//!
//! The request hashes follow the admin-surface discipline: credential
//! material (the force path's `operator_authorization`) is excluded so
//! a replay after token rotation still resolves to the recorded
//! outcome, while every semantic field (the borrower, the expected
//! generations, the residue-check directive) is folded in — a
//! differing body under the same `operation_id` is a typed
//! `IDEMPOTENCY_CONFLICT`, never a silent second attempt.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::ApiError;
use crate::id::{CellId, DeviceId, OperationId};

/// LendDevice request (the rook-cell contract's operator surface).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LendDeviceRequest {
    /// Must equal `volvisor.volume.v2`.
    pub api_version: String,
    /// Idempotency key.
    pub operation_id: OperationId,
    /// The cell borrowing the device. The lend is recorded against
    /// this identity; reclaim observations name it back.
    pub borrower_cell_id: CellId,
    /// The claim generation the operator expects; a mismatch is a
    /// typed `STALE_GENERATION` refusal (the claim record moved).
    pub expected_claim_generation: crate::domain::Generation,
}

impl LendDeviceRequest {
    /// Validate the envelope.
    ///
    /// # Errors
    /// Returns [`ApiError`] when `api_version` is unsupported.
    /// `borrower_cell_id` is a validated newtype, so its charset was
    /// enforced at deserialization.
    pub fn validate(&self) -> Result<(), ApiError> {
        crate::validate_api_version(&self.api_version)
    }

    /// Canonical request hash for idempotency, folded with the target
    /// device over the full immutable body (the borrower and the
    /// expected claim generation included — a differing body under
    /// the same `operation_id` is a conflict). No credential material
    /// appears in a lend request.
    #[must_use]
    pub fn request_hash(&self, device_id: &DeviceId) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(b"volvisor.volume.v2:device_lend:");
        hasher.update(device_id.as_str().as_bytes());
        hasher.update(b":");
        let body = serde_json::to_vec(self).unwrap_or_default();
        hasher.update(&body);
        hasher.finalize().into()
    }
}

/// ReclaimDevice request (the rook-cell contract's operator surface).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReclaimDeviceRequest {
    /// Must equal `volvisor.volume.v2`.
    pub api_version: String,
    /// Idempotency key.
    pub operation_id: OperationId,
    /// The lend generation the operator expects; a mismatch is a
    /// typed `STALE_GENERATION` refusal (the live lend moved).
    pub expected_lend_generation: crate::domain::Generation,
    /// The operator-authorized force path (see [`ForceReclaim`]).
    /// Absent, the reclaim requires the borrower's release to be
    /// observed first.
    pub force: Option<ForceReclaim>,
}

impl ReclaimDeviceRequest {
    /// Validate the envelope.
    ///
    /// # Errors
    /// Returns [`ApiError`] when `api_version` is unsupported or a
    /// present `force` carries an empty authorization token.
    pub fn validate(&self) -> Result<(), ApiError> {
        crate::validate_api_version(&self.api_version)?;
        if let Some(force) = &self.force {
            if force.operator_authorization.is_empty() {
                return Err(ApiError::invalid_request(
                    "force.operator_authorization must not be empty",
                ));
            }
        }
        Ok(())
    }

    /// Canonical request hash for idempotency: the target device, the
    /// operation id, the expected lend generation and the force
    /// directive — **not** the force's `operator_authorization`
    /// (credential material; a re-issued force after token rotation
    /// for the same act is a legitimate replay, exactly like a
    /// re-issued claim).
    #[must_use]
    pub fn request_hash(&self, device_id: &DeviceId) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(b"volvisor.volume.v2:device_reclaim:");
        hasher.update(device_id.as_str().as_bytes());
        hasher.update(b":");
        hasher.update(self.operation_id.as_str().as_bytes());
        hasher.update(b":");
        hasher.update(self.expected_lend_generation.0.to_le_bytes());
        hasher.update(match &self.force {
            None => b"force=none".as_slice(),
            // The directive is part of the immutable act; the token is
            // not (see the method docs).
            Some(_) => b"force=inspect_and_record".as_slice(),
        });
        hasher.finalize().into()
    }
}

/// The operator-authorized force option of [`ReclaimDeviceRequest`].
///
/// Force reclaim is an explicit recorded operation, not a cleanup
/// shortcut: it carries the operator's scoped authorization and the
/// explicit demand that a residue check runs and is recorded before
/// the reclaim completes. Foreign residue quarantines the device —
/// never a silent reuse.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForceReclaim {
    /// Scoped operator-authorization token; compared by the provider
    /// against its configured token (when one is configured) and
    /// never logged or journaled verbatim — the journal pipeline
    /// redacts it like every credential.
    pub operator_authorization: String,
    /// The residue-check directive. The only honest value is
    /// `inspect_and_record`: the observation itself is performed by
    /// the provider's device-surface observer and its result is
    /// carried by the response and the journaled outcome.
    pub residue_check: ResidueCheck,
}

/// The residue-check directive of a force reclaim.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResidueCheck {
    /// Inspect the device's durable surface and record the
    /// observation before the reclaim completes.
    InspectAndRecord,
}

/// ResolveQuarantine request — the explicit operator action that
/// exits a device's quarantine (P9-A's recorded exit decision).
///
/// The resolution reconciles the journaled intent (a crash between a
/// lend/reclaim journal and its effect) or records the operator's
/// inspected-and-accepted judgment on foreign residue. An unreadable
/// ownership record has **no** exit: the refusal names the manual
/// remediation (fail-closed by design).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolveQuarantineRequest {
    /// Must equal `volvisor.volume.v2`.
    pub api_version: String,
    /// Idempotency key.
    pub operation_id: OperationId,
    /// Scoped operator-authorization token; compared by the provider,
    /// never logged or journaled verbatim.
    pub operator_authorization: String,
}

impl ResolveQuarantineRequest {
    /// Validate the envelope.
    ///
    /// # Errors
    /// Returns [`ApiError`] when `api_version` is unsupported or the
    /// authorization token is empty.
    pub fn validate(&self) -> Result<(), ApiError> {
        crate::validate_api_version(&self.api_version)?;
        if self.operator_authorization.is_empty() {
            return Err(ApiError::invalid_request(
                "operator_authorization must not be empty",
            ));
        }
        Ok(())
    }

    /// Canonical request hash for idempotency, folded with the target
    /// device. The authorization token is excluded (credential
    /// material; see [`ReclaimDeviceRequest::request_hash`]).
    #[must_use]
    pub fn request_hash(&self, device_id: &DeviceId) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(b"volvisor.volume.v2:device_resolve_quarantine:");
        hasher.update(device_id.as_str().as_bytes());
        hasher.update(b":");
        hasher.update(self.operation_id.as_str().as_bytes());
        hasher.finalize().into()
    }
}

/// DeviceOwnership response: the typed answer to "who holds this
/// device, and is any of it lent, to whom" (the rook-cell contract's
/// operator-visible shape, extended with the quarantine state P9-A
/// records honestly).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceOwnershipResponse {
    /// The device asked about.
    pub device_id: DeviceId,
    /// Whether volvisor's claim holds the device.
    pub claimed_by: ClaimedBy,
    /// The claim record's generation (bumped by every claim/release
    /// cycle; the lend's `expected_claim_generation` guards against
    /// it).
    pub claim_generation: crate::domain::Generation,
    /// The live lend, when one exists. `null` otherwise — an honest
    /// absence, never a missing field.
    pub lend: Option<LendState>,
    /// The quarantine state, when the device is quarantined (an
    /// unresolved lend/reclaim intent, foreign residue, or an
    /// unreadable ownership record). `null` otherwise.
    pub quarantined: Option<QuarantineState>,
}

/// Who holds the device's claim.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimedBy {
    /// Volvisor's exclusive claim holds the device.
    Volvisor,
    /// The device is known but not claimed.
    Unclaimed,
    /// The ownership record cannot be read, so the claim state is
    /// unknown (the device is quarantined; see
    /// [`QuarantineReason::UnreadableRecord`]). `claim_generation`
    /// is meaningless in this state and reports `0`.
    Unknown,
}

/// The live lend sub-record of the ownership record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LendState {
    /// The borrowing cell.
    pub borrower_cell_id: CellId,
    /// The lend generation (monotonic across lend/reclaim cycles of
    /// the one ownership record; the reclaim's
    /// `expected_lend_generation` guards against it).
    pub lend_generation: crate::domain::Generation,
    /// Unix epoch seconds at recording.
    pub recorded_at: u64,
    /// Optional evidence reference. Not populated by this slice: the
    /// POC's evidence discipline (P9-B) owns the real references.
    pub evidence_ref: Option<String>,
}

/// The quarantine sub-record of the ownership record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuarantineState {
    /// Why the device is quarantined (typed).
    pub reason: QuarantineReason,
    /// Unix epoch seconds at recording.
    pub recorded_at: u64,
}

/// The typed quarantine reasons.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum QuarantineReason {
    /// A lend was journaled but its effect never landed (a crash
    /// between the lend journal and the record mutation). The device
    /// is unlent; the quarantine holds until the intent is
    /// reconciled.
    UnresolvedLendIntent {
        /// The lend operation whose effect is unresolved.
        operation_id: OperationId,
        /// The cell the interrupted lend named.
        borrower_cell_id: CellId,
    },
    /// A reclaim was journaled but its effect never landed. The lend
    /// is still live; the quarantine holds until the intent is
    /// reconciled.
    UnresolvedReclaimIntent {
        /// The reclaim operation whose effect is unresolved.
        operation_id: OperationId,
    },
    /// The device's surface showed a signature that is not the lent
    /// borrower's. Never silently reused; the exit is the operator's
    /// explicit inspected-and-accepted resolution.
    ForeignResidue {
        /// What the observation found (recorded verbatim).
        details: String,
    },
    /// The device's ownership record cannot be read. No automated
    /// exit: the refusal names the manual remediation.
    UnreadableRecord {
        /// Why the record is unreadable (recorded verbatim).
        details: String,
    },
}

/// One observation over a device's durable surface — the release and
/// residue seam's output (see the provider crate's
/// `DeviceSurfaceObserver` for the seam's contract).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeviceSurfaceObservation {
    /// The named borrower's signature is present: the borrower still
    /// holds (or last held) the device.
    BorrowerSignature {
        /// The borrower whose signature was observed.
        borrower_cell_id: CellId,
    },
    /// No borrower signature is observable: the surface is released
    /// (wiped or never written by a borrower).
    Released,
    /// A signature that is not the lent borrower's: foreign residue.
    Foreign {
        /// What was observed (recorded verbatim).
        details: String,
    },
}

/// ReclaimDevice acknowledgment: the reclaim completed, and what the
/// release/residue observation found is recorded with it (both the
/// release path and the force path carry their observation — a
/// reclaim without a recorded justification never happens).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReclaimDeviceAck {
    /// The reclaimed device.
    pub reclaimed: DeviceId,
    /// The observation the reclaim acted on.
    pub observation: DeviceObservation,
}

/// One recorded device-surface observation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceObservation {
    /// What the seam observed.
    pub observed: DeviceSurfaceObservation,
    /// Unix epoch seconds at the observation.
    pub recorded_at: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Generation;
    use crate::id::{CellId, DeviceId, OperationId};

    fn device() -> DeviceId {
        DeviceId::new("dev-1").expect("id")
    }

    fn lend_req() -> LendDeviceRequest {
        serde_json::from_value(serde_json::json!({
            "api_version": "volvisor.volume.v2",
            "operation_id": "op-lend-1",
            "borrower_cell_id": "cell-a",
            "expected_claim_generation": 3,
        }))
        .expect("valid")
    }

    #[test]
    fn lend_validates_and_folds_the_body() {
        let req = lend_req();
        assert!(req.validate().is_ok());
        let mut req = lend_req();
        req.api_version = "volvisor.volume.v1".to_owned();
        assert!(req.validate().is_err());

        // The full immutable body is in the hash: a different
        // borrower or expectation under the same operation id is a
        // conflict, and the device is folded in.
        let a = lend_req();
        let mut other_borrower = lend_req();
        other_borrower.borrower_cell_id = CellId::new("cell-b").expect("id");
        let mut other_generation = lend_req();
        other_generation.expected_claim_generation = Generation(4);
        assert_ne!(
            a.request_hash(&device()),
            other_borrower.request_hash(&device())
        );
        assert_ne!(
            a.request_hash(&device()),
            other_generation.request_hash(&device())
        );
        assert_eq!(
            a.request_hash(&device()),
            lend_req().request_hash(&device())
        );
        let other_device = DeviceId::new("dev-2").expect("id");
        assert_ne!(a.request_hash(&device()), a.request_hash(&other_device));
    }

    #[test]
    fn lend_rejects_unknown_fields() {
        let json = serde_json::json!({
            "api_version": "volvisor.volume.v2",
            "operation_id": "op-l",
            "borrower_cell_id": "cell-a",
            "expected_claim_generation": 1,
            "surprise": true
        });
        assert!(serde_json::from_value::<LendDeviceRequest>(json).is_err());
    }

    fn reclaim_req(force: Option<&str>) -> ReclaimDeviceRequest {
        let force_json = force.map(|token| {
            serde_json::json!({
                "operator_authorization": token,
                "residue_check": "inspect_and_record"
            })
        });
        serde_json::from_value(serde_json::json!({
            "api_version": "volvisor.volume.v2",
            "operation_id": "op-reclaim-1",
            "expected_lend_generation": 2,
            "force": force_json,
        }))
        .expect("valid")
    }

    #[test]
    fn reclaim_hash_excludes_the_token_but_not_the_act() {
        let plain = reclaim_req(None);
        assert!(plain.validate().is_ok());

        let forced = reclaim_req(Some("token-a"));
        assert!(forced.validate().is_ok());
        let rotated = reclaim_req(Some("token-b"));

        // The token is credential material: rotation replays.
        assert_eq!(
            forced.request_hash(&device()),
            rotated.request_hash(&device())
        );
        // The act is immutable: force-vs-plain and the expected
        // generation are folded in.
        assert_ne!(
            plain.request_hash(&device()),
            forced.request_hash(&device())
        );
        let mut other_generation = plain.clone();
        other_generation.expected_lend_generation = Generation(3);
        assert_ne!(
            plain.request_hash(&device()),
            other_generation.request_hash(&device())
        );

        // An empty force token is a validation-level rejection.
        let empty = reclaim_req(Some(""));
        assert!(empty.validate().is_err());
    }

    #[test]
    fn residue_check_has_one_directive() {
        let json = serde_json::json!("inspect_and_record");
        assert_eq!(
            serde_json::from_value::<ResidueCheck>(json).expect("directive"),
            ResidueCheck::InspectAndRecord
        );
        assert!(serde_json::from_value::<ResidueCheck>(serde_json::json!("skip")).is_err());
    }

    #[test]
    fn resolve_hash_excludes_the_token() {
        let req: ResolveQuarantineRequest = serde_json::from_value(serde_json::json!({
            "api_version": "volvisor.volume.v2",
            "operation_id": "op-resolve-1",
            "operator_authorization": "token-a",
        }))
        .expect("valid");
        assert!(req.validate().is_ok());
        let mut rotated = req.clone();
        rotated.operator_authorization = "token-b".to_owned();
        assert_eq!(req.request_hash(&device()), rotated.request_hash(&device()));

        let mut empty = req.clone();
        empty.operator_authorization = String::new();
        assert!(empty.validate().is_err());
    }

    #[test]
    fn ownership_shape_round_trips() {
        let response: DeviceOwnershipResponse = serde_json::from_value(serde_json::json!({
            "device_id": "dev-1",
            "claimed_by": "volvisor",
            "claim_generation": 3,
            "lend": {
                "borrower_cell_id": "cell-a",
                "lend_generation": 2,
                "recorded_at": 1_760_000_000,
                "evidence_ref": null
            },
            "quarantined": {
                "reason": {
                    "kind": "unresolved_lend_intent",
                    "operation_id": "op-lend-1",
                    "borrower_cell_id": "cell-a"
                },
                "recorded_at": 1_760_000_001
            }
        }))
        .expect("valid");
        assert_eq!(response.claimed_by, ClaimedBy::Volvisor);
        let lend = response.lend.expect("lend present");
        assert_eq!(lend.lend_generation, Generation(2));
        assert_eq!(lend.evidence_ref, None);
        let quarantine = response.quarantined.expect("quarantine present");
        assert_eq!(
            quarantine.reason,
            QuarantineReason::UnresolvedLendIntent {
                operation_id: OperationId::new("op-lend-1").expect("id"),
                borrower_cell_id: CellId::new("cell-a").expect("id"),
            }
        );

        // The honest-absence shape: explicit nulls, not missing fields.
        let unclaimed: DeviceOwnershipResponse = serde_json::from_value(serde_json::json!({
            "device_id": "dev-1",
            "claimed_by": "unclaimed",
            "claim_generation": 0,
            "lend": null,
            "quarantined": null
        }))
        .expect("valid");
        assert_eq!(unclaimed.claimed_by, ClaimedBy::Unclaimed);
        assert!(unclaimed.lend.is_none());
        assert!(unclaimed.quarantined.is_none());
    }
}
