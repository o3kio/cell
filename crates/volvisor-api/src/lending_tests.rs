//! The device-lending Tier S suite (P9-A): the six invariants of the
//! rook-cell contract's lending surface, each driven to its own test
//! through the real router over the durable fake.
//!
//! These are daemon-level tests — the same discipline as the crash
//! campaign's rows, in-crate like `tests.rs` (the plan asks for a
//! Tier S test of each invariant, not a campaign registration; the
//! campaign's `recorded_gaps` entries cite this module as the
//! carrier, since its DRBD rig has no lending surface to drive).
//!
//! ## The harness
//!
//! Each test owns a [`LendingRig`]: the durable composition
//! (`FakeProvider::with_durable_devices` over a state file, the
//! journal, both crash-hook tables) served through the real router
//! with `tower::ServiceExt::oneshot`. A crash-window test arms the
//! ownership store's seam (or the API journal's), fires the request
//! in a spawned task — the armed point terminates that task with a
//! [`crate::CRASH_PANIC_PREFIX`] panic, so the `JoinError` IS the
//! kill — then *relaunches*: the previous `AppState` is dropped
//! first (the journal flocks its directory), the world reloads its
//! state file, and the restarted daemon must answer for exactly
//! what the durable artifacts prove.
//!
//! The fake's modeled device surface (what the borrower leaves
//! observable) is in-memory and resets to *released* on every
//! launch — deliberately: the durable truth is the ownership record,
//! and the reconciliation must honor the *journaled* observation,
//! never a re-observation after the fact.
//!
//! ## The invariant map
//!
//! | Invariant (contract, "Device lending and sharing") | Test |
//! |---|---|
//! | 1. Lend is journaled, idempotent by operation id + body hash; typed conflicts; stale-generation fencing | `invariant_1_*` |
//! | 2. Claim not surrendered: claim, lend and reclaim are generations of one record; no double ownership | `invariant_2_*` |
//! | 3. The lent state is operator-visible in `DeviceOwnership` | `invariant_3_*` |
//! | 4. A lent (or quarantined) device is frozen for volvisor mutations, typed refusals naming the live lend | `invariant_4_*` |
//! | 5. Reclaim needs an observed release or an operator-authorized force with a recorded residue check; foreign residue quarantines | `invariant_5_*` |
//! | 6. Unknown state fails closed: crash windows quarantine; unreadable records quarantine with no automated exit | `invariant_6_*` (four windows) |

// Test bodies favor linear readability over clippy's size heuristics.
#![allow(clippy::too_many_lines)]

use std::sync::Arc;
use std::sync::Once;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use serde_json::{Value, json};
use tokio::task::JoinError;
use tower::ServiceExt;
use volvisor_journal::Journal;
use volvisor_provider::fake::FakeProvider;
use volvisor_provider::{AdminSurface, FakeDeviceWorld, LendingSurface};
use volvisor_types::crash::{STORE_DEVICE_OWNERSHIP, StoreCrashHooks, StoreSavePoint};
use volvisor_types::{CellId, DeviceId};

use crate::crash::{CrashHooks, CrashPoint};
use crate::{ApiConfig, AppState, SharedState, ops, router};

/// Admin bearer token of the rig's daemon.
const ADMIN_TOKEN: &str = "test-admin-token";
/// The scoped operator-authorization token the rig's world compares
/// the force/resolve paths against (the two-token model).
const OPERATOR_TOKEN: &str = "operator-force-token";
/// The single device of the fake's durable world.
const DEVICE: &str = "dev-fake-1";
/// The borrowing cell used throughout.
const CELL_A: &str = "cell-a";

// --------------------------------------------------- the panic filter

/// This module's crash-injection filter (the rig precedent): a
/// firing hook terminates its request task with a
/// [`crate::CRASH_PANIC_PREFIX`] payload — an injected kill, not a
/// failure — so those panics print nothing while every real panic
/// still reaches the previous hook.
static PANIC_FILTER: Once = Once::new();

fn install_panic_filter() {
    PANIC_FILTER.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let injected = info
                .payload()
                .downcast_ref::<String>()
                .is_some_and(|payload| payload.starts_with(crate::CRASH_PANIC_PREFIX));
            if !injected {
                previous(info);
            }
        }));
    });
}

/// The device identity every testkit call targets.
fn device() -> DeviceId {
    DeviceId::new(DEVICE).expect("fake device id")
}

/// The borrowing cell identity.
fn cell(id: &str) -> CellId {
    CellId::new(id).expect("cell id")
}

fn app(state: &SharedState) -> Router {
    router(state.clone(), ApiConfig::default().max_body_bytes)
}

// ------------------------------------------------------------------
// The rig
// ------------------------------------------------------------------

/// One daemon incarnation: the durable composition, served through
/// the real router.
struct Launch {
    state: SharedState,
    world: Arc<FakeDeviceWorld>,
    app: Router,
}

/// The durable lending rig: the state directory (journal + ownership
/// record), the two crash-hook tables (shared across launches — arms
/// are one-shot and consumed by the request they kill), and the
/// current launch.
struct LendingRig {
    dir: tempfile::TempDir,
    crash: Arc<CrashHooks>,
    store_crash: Arc<StoreCrashHooks>,
    launch: Option<Launch>,
}

impl LendingRig {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().expect("temporary directory"),
            crash: Arc::new(CrashHooks::new()),
            store_crash: Arc::new(StoreCrashHooks::new()),
            launch: None,
        }
    }

    /// Start (or restart) the daemon: the previous incarnation is
    /// dropped FIRST — the journal flocks its directory, so the old
    /// `AppState` must be gone before the new one opens. The world
    /// reloads the ownership record from the state file, which is
    /// what makes the crash windows provable.
    fn start(&mut self) {
        self.launch = None;
        let journal = Journal::open(self.dir.path()).expect("journal open");
        let provider = Arc::new(FakeProvider::new().with_durable_devices(
            self.dir.path().join("device-ownership.json"),
            Arc::clone(&self.store_crash),
            Some(OPERATOR_TOKEN.to_owned()),
        ));
        let admin: Arc<dyn AdminSurface> = provider.clone();
        let lending: Arc<dyn LendingSurface> = provider.clone();
        let state = Arc::new(
            AppState::new(
                provider.clone(),
                Some(admin),
                journal,
                Some(ADMIN_TOKEN.to_owned()),
            )
            .with_lending(lending)
            .with_crash_hooks(Arc::clone(&self.crash)),
        );
        let world = provider
            .durable_devices()
            .expect("the durable device world");
        let built = app(&state);
        self.launch = Some(Launch {
            state,
            world,
            app: built,
        });
    }

    /// The current launch (panics when not started).
    fn launch(&self) -> &Launch {
        self.launch.as_ref().expect("the rig is started")
    }

    /// The current launch's device world (the testkit controls).
    fn world(&self) -> Arc<FakeDeviceWorld> {
        Arc::clone(&self.launch().world)
    }

    /// The current launch's journal record count (the
    /// journal-before-mutate observable).
    fn journal_records(&self) -> u64 {
        self.launch()
            .state
            .journal
            .lock()
            .expect("journal lock in test")
            .record_count()
    }

    fn app(&self) -> &Router {
        &self.launch().app
    }

    async fn send(&self, method: Method, uri: &str, body: &Value) -> (StatusCode, Value) {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::AUTHORIZATION, format!("Bearer {ADMIN_TOKEN}"))
            .body(Body::from(body.to_string()))
            .expect("build request");
        let response = self
            .app()
            .clone()
            .oneshot(request)
            .await
            .expect("router call");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read response body");
        let text = String::from_utf8(bytes.to_vec()).expect("utf-8 body");
        let value = serde_json::from_str(&text).expect("JSON response body");
        (status, value)
    }

    // Typed operations over the fake's device.

    async fn claim(&self, op: &str) -> (StatusCode, Value) {
        self.send(
            Method::POST,
            &format!("/v2/admin/devices/{DEVICE}/claim"),
            &json!({
                "api_version": "volvisor.volume.v2",
                "operation_id": op,
                "authorization_token": "ignored-by-the-fake",
            }),
        )
        .await
    }

    async fn release(&self, op: &str) -> (StatusCode, Value) {
        self.send(
            Method::POST,
            &format!("/v2/admin/devices/{DEVICE}/release"),
            &json!({
                "api_version": "volvisor.volume.v2",
                "operation_id": op,
                "authorization_token": "ignored-by-the-fake",
            }),
        )
        .await
    }

    async fn lend(
        &self,
        op: &str,
        borrower: &str,
        expected_claim_generation: u64,
    ) -> (StatusCode, Value) {
        self.send(
            Method::POST,
            &format!("/v2/admin/devices/{DEVICE}/lend"),
            &json!({
                "api_version": "volvisor.volume.v2",
                "operation_id": op,
                "borrower_cell_id": borrower,
                "expected_claim_generation": expected_claim_generation,
            }),
        )
        .await
    }

    async fn reclaim(&self, op: &str, expected_lend_generation: u64) -> (StatusCode, Value) {
        self.reclaim_body(op, expected_lend_generation, json!(null))
            .await
    }

    async fn force_reclaim(
        &self,
        op: &str,
        expected_lend_generation: u64,
        operator_authorization: &str,
    ) -> (StatusCode, Value) {
        self.reclaim_body(
            op,
            expected_lend_generation,
            json!({
                "operator_authorization": operator_authorization,
                "residue_check": "inspect_and_record",
            }),
        )
        .await
    }

    async fn reclaim_body(
        &self,
        op: &str,
        expected_lend_generation: u64,
        force: Value,
    ) -> (StatusCode, Value) {
        self.send(
            Method::POST,
            &format!("/v2/admin/devices/{DEVICE}/reclaim"),
            &json!({
                "api_version": "volvisor.volume.v2",
                "operation_id": op,
                "expected_lend_generation": expected_lend_generation,
                "force": force,
            }),
        )
        .await
    }

    async fn resolve(&self, op: &str, operator_authorization: &str) -> (StatusCode, Value) {
        self.send(
            Method::POST,
            &format!("/v2/admin/devices/{DEVICE}/resolve-quarantine"),
            &json!({
                "api_version": "volvisor.volume.v2",
                "operation_id": op,
                "operator_authorization": operator_authorization,
            }),
        )
        .await
    }

    async fn ownership(&self) -> (StatusCode, Value) {
        self.send(
            Method::GET,
            &format!("/v2/admin/devices/{DEVICE}/ownership"),
            &json!({}),
        )
        .await
    }

    async fn discover(&self) -> (StatusCode, Value) {
        self.send(Method::GET, "/v2/admin/devices", &json!({}))
            .await
    }
}

/// Fire one request in its own task and return its outcome — a kill
/// fired by an armed crash point surfaces as the `JoinError`, which
/// is the injection's observable.
async fn fire(app: Router, method: Method, uri: &str, body: &Value) -> Result<(), JoinError> {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, format!("Bearer {ADMIN_TOKEN}"))
        .body(Body::from(body.to_string()))
        .expect("build request");
    tokio::task::spawn(async move {
        let _ = app
            .oneshot(request)
            .await
            .expect("router call before the kill");
    })
    .await
}

/// Assert a `JoinError` is the *injected* kill (the armed crash
/// point), not a real panic.
fn assert_injected_kill(err: JoinError) {
    assert!(err.is_panic(), "the request must die to the armed kill");
    let payload = err.into_panic();
    let message = payload
        .downcast_ref::<String>()
        .map_or("<non-string panic payload>", String::as_str);
    assert!(
        message.starts_with(crate::CRASH_PANIC_PREFIX),
        "the kill must be the injected crash point, not a real panic: {message}"
    );
}

/// Assert a typed error body with its status.
fn assert_refusal(body: &Value, status: StatusCode, code: &str) {
    assert_eq!(
        status,
        code_status(code),
        "the refusal {code} must carry its status"
    );
    assert_eq!(body["code"], json!(code), "unexpected body: {body}");
}

/// The HTTP status of a wire code (mirrors the taxonomy's mapping).
fn code_status(code: &str) -> StatusCode {
    match code {
        "INVALID_REQUEST" | "UNSUPPORTED_CLASS_OR_POLICY" => StatusCode::BAD_REQUEST,
        "NOT_FOUND" => StatusCode::NOT_FOUND,
        "INTERNAL" => StatusCode::INTERNAL_SERVER_ERROR,
        // Every fencing/state conflict family member.
        _ => StatusCode::CONFLICT,
    }
}

/// Claim the device, asserting success, and return the claim
/// generation the next lend must expect.
async fn claimed(rig: &LendingRig) -> u64 {
    let (status, body) = rig.claim("op-claim").await;
    assert_eq!(status, StatusCode::OK, "claim must succeed: {body}");
    1
}

/// Lend to `cell-a`, asserting success, and return the lend
/// generation.
async fn lent(rig: &LendingRig, op: &str, expected_claim_generation: u64) -> u64 {
    let (status, body) = rig.lend(op, CELL_A, expected_claim_generation).await;
    assert_eq!(status, StatusCode::OK, "lend must succeed: {body}");
    body["lend"]["lend_generation"]
        .as_u64()
        .expect("the lend generation")
}

// ------------------------------------------------------------------
// Invariant 1
// ------------------------------------------------------------------

/// **Invariant 1**: the lend is a journaled, idempotent mutation —
/// replays return the recorded response; a differing body under the
/// same `operation_id` is a typed `IDEMPOTENCY_CONFLICT`; the
/// expected-generation guards are typed `STALE_GENERATION`
/// refusals; the reclaim family replays token-rotation-tolerantly
/// (the hash excludes the credential, so the journal answers before
/// the token is ever compared again).
#[tokio::test]
async fn invariant_1_lend_is_journaled_idempotent_and_typed() {
    let mut rig = LendingRig::new();
    rig.start();
    let claim_generation = claimed(&rig).await;

    // The expected claim generation guards the lend while its slot is
    // free (once a lend is live, the more specific double-lend
    // refusal answers first).
    let (status, stale) = rig
        .lend("op-lend-stale", CELL_A, claim_generation + 7)
        .await;
    assert_refusal(&stale, status, "STALE_GENERATION");

    // The journal carries the claim's intent+outcome before the
    // lend runs (journal-before-mutate at the API layer).
    let before = rig.journal_records();
    let (status, lend) = rig.lend("op-lend-1", CELL_A, claim_generation).await;
    assert_eq!(status, StatusCode::OK, "lend must succeed: {lend}");
    assert_eq!(lend["claimed_by"], json!("volvisor"));
    assert_eq!(lend["claim_generation"], json!(claim_generation));
    assert_eq!(lend["lend"]["borrower_cell_id"], json!(CELL_A));
    assert_eq!(lend["lend"]["lend_generation"], json!(1));
    assert!(lend["lend"]["recorded_at"].as_u64().is_some_and(|t| t > 0));
    assert_eq!(lend["lend"]["evidence_ref"], json!(null));
    assert_eq!(lend["quarantined"], json!(null));
    assert!(rig.journal_records() > before, "the lend must journal");

    // An identical replay returns the recorded response — byte
    // equal, not a second execution.
    let (status, replay) = rig.lend("op-lend-1", CELL_A, claim_generation).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(replay, lend, "a replay must return the recorded response");

    // A differing body under the same operation id is the typed
    // conflict, never a silent second lend.
    let (status, conflict) = rig.lend("op-lend-1", "cell-b", claim_generation).await;
    assert_refusal(&conflict, status, "IDEMPOTENCY_CONFLICT");

    // The original outcome is intact after the conflict.
    let (status, again) = rig.lend("op-lend-1", CELL_A, claim_generation).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again, lend);

    // The reclaim family: the observed release completes, and the
    // force path replays across token rotation (the credential is
    // not part of the immutable act).
    let world = rig.world();
    world
        .model_borrower_release(&device())
        .expect("model release");
    let (status, ack) = rig.reclaim("op-reclaim-1", 1).await;
    assert_eq!(status, StatusCode::OK, "reclaim must succeed: {ack}");
    assert_eq!(ack["reclaimed"], json!(DEVICE));
    assert_eq!(ack["observation"]["observed"]["kind"], json!("released"));

    let lend_generation = lent(&rig, "op-lend-2", claim_generation).await;
    let (status, forced) = rig
        .force_reclaim("op-force-1", lend_generation, OPERATOR_TOKEN)
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "force reclaim must succeed: {forced}"
    );
    // The force path over the released surface completes and records
    // the clean observation it acted on (the unclean-exit force — the
    // borrower still holding — is invariant 5's case).
    assert_eq!(forced["observation"]["observed"]["kind"], json!("released"));

    // The rotated-token replay resolves to the recorded outcome
    // before the provider ever compares the new credential.
    let (status, rotated) = rig
        .force_reclaim("op-force-1", lend_generation, "rotated-token")
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        rotated, forced,
        "a token-rotated replay must return the recorded outcome"
    );

    // A differing force body (the expected generation) under the
    // same operation id is the typed conflict.
    let (status, conflict) = rig
        .force_reclaim("op-force-1", lend_generation + 1, OPERATOR_TOKEN)
        .await;
    assert_refusal(&conflict, status, "IDEMPOTENCY_CONFLICT");

    // An empty force token is a validation-level rejection.
    let lend_generation = lent(&rig, "op-lend-3", claim_generation).await;
    let (status, empty) = rig.force_reclaim("op-force-2", lend_generation, "").await;
    assert_refusal(&empty, status, "INVALID_REQUEST");

    // The expected lend generation guards the reclaim.
    let (status, stale) = rig.reclaim("op-reclaim-2", lend_generation + 9).await;
    assert_refusal(&stale, status, "STALE_GENERATION");
}

// ------------------------------------------------------------------
// Invariant 2
// ------------------------------------------------------------------

/// **Invariant 2**: the claim is never surrendered — the claim, the
/// lend and the reclaim are generations of *one* ownership record,
/// with no double-ownership window. The observable: `claimed_by`
/// stays `volvisor` through the lend and the reclaim, the claim
/// generation moves only on claim/release cycles, and the lend
/// generation is monotonic across cycles.
#[tokio::test]
async fn invariant_2_claim_lend_reclaim_are_generations_of_one_record() {
    let mut rig = LendingRig::new();
    rig.start();

    // The fresh record: known, unclaimed, generation 0.
    let (status, fresh) = rig.ownership().await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fresh["claimed_by"], json!("unclaimed"));
    assert_eq!(fresh["claim_generation"], json!(0));
    assert_eq!(fresh["lend"], json!(null));
    assert_eq!(fresh["quarantined"], json!(null));

    let (status, _) = rig.claim("op-claim").await;
    assert_eq!(status, StatusCode::OK);
    let (status, ownership) = rig.ownership().await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ownership["claimed_by"], json!("volvisor"));
    assert_eq!(ownership["claim_generation"], json!(1));

    // The lend does not move the claim generation, and does not
    // replace the claim: one record, one optional lend sub-record.
    let (status, _) = rig.lend("op-lend-1", CELL_A, 1).await;
    assert_eq!(status, StatusCode::OK);
    let (status, ownership) = rig.ownership().await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ownership["claimed_by"], json!("volvisor"));
    assert_eq!(
        ownership["claim_generation"],
        json!(1),
        "the lend is not a claim cycle"
    );
    assert_eq!(ownership["lend"]["lend_generation"], json!(1));

    // The reclaim does not move it either.
    let world = rig.world();
    world
        .model_borrower_release(&device())
        .expect("model release");
    let (status, _) = rig.reclaim("op-reclaim-1", 1).await;
    assert_eq!(status, StatusCode::OK);
    let (status, ownership) = rig.ownership().await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ownership["claimed_by"], json!("volvisor"));
    assert_eq!(
        ownership["claim_generation"],
        json!(1),
        "the reclaim is not a claim cycle"
    );
    assert_eq!(ownership["lend"], json!(null));

    // The next lend gets a fresh lend generation — never a reuse.
    let (status, _) = rig.lend("op-lend-2", CELL_A, 1).await;
    assert_eq!(status, StatusCode::OK);
    let (status, ownership) = rig.ownership().await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ownership["lend"]["lend_generation"], json!(2));

    // The release and the next claim advance the SAME record's claim
    // generation — the cycle the next lend must expect.
    let (status, _) = rig.reclaim("op-reclaim-2", 2).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = rig.release("op-release-1").await;
    assert_eq!(status, StatusCode::OK);
    let (status, ownership) = rig.ownership().await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ownership["claimed_by"], json!("unclaimed"));
    assert_eq!(
        ownership["claim_generation"],
        json!(2),
        "the release bumps the one record"
    );

    let (status, _) = rig.claim("op-claim-2").await;
    assert_eq!(status, StatusCode::OK);
    let (status, ownership) = rig.ownership().await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ownership["claim_generation"], json!(3));

    // The next lend is fenced by the new claim generation (a fresh
    // operation id: the stale attempt's refusal is itself a recorded
    // terminal outcome, exactly like every journaled mutation).
    let (status, stale) = rig.lend("op-lend-stale", CELL_A, 1).await;
    assert_refusal(&stale, status, "STALE_GENERATION");
    let (status, _) = rig.lend("op-lend-3", CELL_A, 3).await;
    assert_eq!(status, StatusCode::OK);
}

// ------------------------------------------------------------------
// Invariant 3
// ------------------------------------------------------------------

/// **Invariant 3**: the lent state is operator-visible —
/// `DeviceOwnership` answers the full lend sub-record (the borrower,
/// the lend generation, the recording time) while it holds, and the
/// honest nulls when it does not; the answer is read-only.
#[tokio::test]
async fn invariant_3_the_lend_is_operator_visible_in_ownership() {
    let mut rig = LendingRig::new();
    rig.start();
    let claim_generation = claimed(&rig).await;
    let lend_generation = lent(&rig, "op-lend-1", claim_generation).await;

    let (status, ownership) = rig.ownership().await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ownership["device_id"], json!(DEVICE));
    assert_eq!(ownership["claimed_by"], json!("volvisor"));
    assert_eq!(ownership["claim_generation"], json!(claim_generation));
    let lend = &ownership["lend"];
    assert_eq!(lend["borrower_cell_id"], json!(CELL_A));
    assert_eq!(lend["lend_generation"], json!(lend_generation));
    assert!(lend["recorded_at"].as_u64().is_some_and(|t| t > 0));
    assert_eq!(lend["evidence_ref"], json!(null));
    assert_eq!(ownership["quarantined"], json!(null));

    // The answer is read-only: repeated reads are stable, and no
    // journal record appears for them.
    let records = rig.journal_records();
    let (status, again) = rig.ownership().await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again, ownership);
    assert_eq!(
        rig.journal_records(),
        records,
        "ownership reads do not journal"
    );

    // An unknown device is the typed 404, never an invented record.
    let (status, body) = rig
        .send(
            Method::GET,
            "/v2/admin/devices/dev-unknown/ownership",
            &json!({}),
        )
        .await;
    assert_refusal(&body, status, "NOT_FOUND");
}

// ------------------------------------------------------------------
// Invariant 4
// ------------------------------------------------------------------

/// **Invariant 4**: a lent device is frozen for volvisor's own
/// mutations — a second lend, a release and a re-claim are typed
/// refusals naming the live lend. The quarantined freeze is exercised
/// by the invariant-6 windows.
#[tokio::test]
async fn invariant_4_a_lent_device_is_frozen_for_volvisor_mutations() {
    let mut rig = LendingRig::new();
    rig.start();
    let claim_generation = claimed(&rig).await;
    let lend_generation = lent(&rig, "op-lend-1", claim_generation).await;

    // A second lend — same borrower or another — is refused naming
    // the live lend.
    let (status, refusal) = rig.lend("op-lend-2", "cell-b", claim_generation).await;
    assert_refusal(&refusal, status, "WRITER_ALREADY_ACTIVE");
    assert!(
        refusal["message"]
            .as_str()
            .is_some_and(|m| m.contains(CELL_A) && m.contains("lend generation 1")),
        "the refusal must name the live lend: {refusal}"
    );
    let (status, refusal) = rig.lend("op-lend-3", CELL_A, claim_generation).await;
    assert_refusal(&refusal, status, "WRITER_ALREADY_ACTIVE");

    // The release is refused while lent (the claim is not
    // surrendered behind the lend's back).
    let (status, refusal) = rig.release("op-release-1").await;
    assert_refusal(&refusal, status, "WRITER_ALREADY_ACTIVE");
    assert!(
        refusal["message"]
            .as_str()
            .is_some_and(|m| m.contains(CELL_A)),
        "the release refusal must name the borrower: {refusal}"
    );

    // The claim route is frozen too: the freeze check runs before the
    // ordinary already-claimed refusal, so a re-claim attempt while
    // lent names the live lend (the claim is not surrendered behind
    // the lend's back, on either route).
    let (status, refusal) = rig.claim("op-claim-2").await;
    assert_refusal(&refusal, status, "WRITER_ALREADY_ACTIVE");
    assert!(
        refusal["message"]
            .as_str()
            .is_some_and(|m| m.contains(CELL_A) && m.contains("lend generation 1")),
        "the claim freeze must name the live lend: {refusal}"
    );

    // The freeze holds the line until the reclaim completes.
    let world = rig.world();
    world
        .model_borrower_release(&device())
        .expect("model release");
    let (status, _) = rig.reclaim("op-reclaim-1", lend_generation).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = rig.release("op-release-2").await;
    assert_eq!(status, StatusCode::OK, "the release follows the reclaim");
}

// ------------------------------------------------------------------
// Invariant 5
// ------------------------------------------------------------------

/// **Invariant 5**: the reclaim requires the borrower's release to
/// be observed, or the operator-authorized force path whose residue
/// check is recorded; foreign residue QUARANTINES — never a silent
/// reuse — and the quarantine exit is the operator's explicit
/// recorded resolution.
#[tokio::test]
async fn invariant_5_reclaim_requires_an_observed_release_or_authorized_force() {
    let mut rig = LendingRig::new();
    rig.start();
    let claim_generation = claimed(&rig).await;
    let lend_generation = lent(&rig, "op-lend-1", claim_generation).await;

    // The borrower still holds: the plain reclaim is refused naming
    // it, and nothing is written.
    let world = rig.world();
    world
        .model_borrower_takes(&device(), &cell(CELL_A))
        .expect("model borrower takes");
    let (status, refusal) = rig.reclaim("op-reclaim-1", lend_generation).await;
    assert_refusal(&refusal, status, "WRITER_ALREADY_ACTIVE");
    assert!(
        refusal["message"]
            .as_str()
            .is_some_and(|m| m.contains(CELL_A)),
        "the refusal must name the borrower: {refusal}"
    );
    let (status, ownership) = rig.ownership().await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        ownership["lend"].is_object(),
        "the lend is untouched by the refusal"
    );
    assert_eq!(ownership["quarantined"], json!(null));

    // The force path demands the operator's scoped authorization.
    let (status, refusal) = rig
        .force_reclaim("op-force-1", lend_generation, "wrong-token")
        .await;
    assert_refusal(&refusal, status, "UNSUPPORTED_CLASS_OR_POLICY");

    // The authorized force over the unclean exit completes and
    // records the residue check it acted on.
    let (status, ack) = rig
        .force_reclaim("op-force-2", lend_generation, OPERATOR_TOKEN)
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the authorized force must complete: {ack}"
    );
    assert_eq!(
        ack["observation"]["observed"]["kind"],
        json!("borrower_signature")
    );
    assert_eq!(
        ack["observation"]["observed"]["borrower_cell_id"],
        json!(CELL_A)
    );
    assert!(
        ack["observation"]["recorded_at"]
            .as_u64()
            .is_some_and(|t| t > 0)
    );
    let (status, ownership) = rig.ownership().await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ownership["lend"], json!(null));
    assert_eq!(ownership["quarantined"], json!(null));

    // Foreign residue on the plain path: the typed refusal, the
    // device quarantined, the lend still live (the reclaim did not
    // complete), and the freeze while quarantined.
    let lend_generation = lent(&rig, "op-lend-2", claim_generation).await;
    world
        .model_foreign_residue(&device(), "foreign zfs superblock")
        .expect("model foreign residue");
    let (status, refusal) = rig.reclaim("op-reclaim-2", lend_generation).await;
    assert_refusal(&refusal, status, "FOREIGN_DEVICE_STATE");
    assert!(
        refusal["message"]
            .as_str()
            .is_some_and(|m| m.contains("foreign zfs superblock") && m.contains("quarantined")),
        "the refusal must record the residue and the quarantine: {refusal}"
    );
    let (status, ownership) = rig.ownership().await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        ownership["lend"].is_object(),
        "foreign residue does not complete the reclaim"
    );
    assert_eq!(
        ownership["quarantined"]["reason"]["kind"],
        json!("foreign_residue")
    );
    assert_eq!(
        ownership["quarantined"]["reason"]["details"],
        json!("foreign zfs superblock")
    );
    let (status, refusal) = rig.release("op-release-1").await;
    assert_refusal(&refusal, status, "INVALID_STATE");
    assert!(
        refusal["message"]
            .as_str()
            .is_some_and(|m| m.contains("quarantined")),
        "the freeze must name the quarantine: {refusal}"
    );

    // The exit is the operator's explicit, authorized resolution —
    // and the wrong token does not get it.
    let (status, refusal) = rig.resolve("op-resolve-1", "wrong-token").await;
    assert_refusal(&refusal, status, "UNSUPPORTED_CLASS_OR_POLICY");
    let (status, resolved) = rig.resolve("op-resolve-2", OPERATOR_TOKEN).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the resolution must succeed: {resolved}"
    );
    assert_eq!(resolved["lend"], json!(null), "the borrower's tenure ends");
    assert_eq!(resolved["quarantined"], json!(null));
    assert_eq!(resolved["claimed_by"], json!("volvisor"), "the claim stays");

    // The resolved device is operable again, and the force path over
    // foreign residue quarantines exactly like the plain path.
    let lend_generation = lent(&rig, "op-lend-3", claim_generation).await;
    world
        .model_foreign_residue(&device(), "another foreign signature")
        .expect("model foreign residue");
    let (status, refusal) = rig
        .force_reclaim("op-force-3", lend_generation, OPERATOR_TOKEN)
        .await;
    assert_refusal(&refusal, status, "FOREIGN_DEVICE_STATE");
    assert!(
        refusal["message"]
            .as_str()
            .is_some_and(|m| m.contains("force reclaim")),
        "the force refusal must name its path: {refusal}"
    );
    let (status, ownership) = rig.ownership().await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        ownership["quarantined"]["reason"]["kind"],
        json!("foreign_residue")
    );
    let (status, _) = rig.resolve("op-resolve-3", OPERATOR_TOKEN).await;
    assert_eq!(status, StatusCode::OK);
}

// ------------------------------------------------------------------
// Invariant 6 — the fail-closed windows
// ------------------------------------------------------------------

/// **Invariant 6 (the lend window)**: a crash between the lend's
/// journal (the ownership record's pending-intent save) and its
/// effect leaves the device **unlent but quarantined** — the state
/// is never inferred from observation; the in-doubt operation never
/// re-executes; the exit is the explicit reconciliation.
#[tokio::test]
async fn invariant_6_crash_between_the_lend_journal_and_its_effect_fails_closed() {
    install_panic_filter();
    let mut rig = LendingRig::new();
    rig.start();
    let claim_generation = claimed(&rig).await;

    // Arm the ownership store's commit boundary: the first save that
    // reaches AfterRename after this point is the lend's *intent*
    // save — the journal-before-mutate half — so the kill lands
    // exactly between the journal and the effect.
    rig.store_crash
        .arm(STORE_DEVICE_OWNERSHIP, StoreSavePoint::AfterRename);
    let err = fire(
        rig.app().clone(),
        Method::POST,
        &format!("/v2/admin/devices/{DEVICE}/lend"),
        &json!({
            "api_version": "volvisor.volume.v2",
            "operation_id": "op-lend-crash",
            "borrower_cell_id": CELL_A,
            "expected_claim_generation": claim_generation,
        }),
    )
    .await
    .expect_err("the lend must die to the armed kill");
    assert_injected_kill(err);

    // The restart sees exactly what the durable artifacts prove: the
    // intent, not the effect — unlent, quarantined.
    rig.start();
    let (status, ownership) = rig.ownership().await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ownership["claimed_by"], json!("volvisor"));
    assert_eq!(ownership["claim_generation"], json!(claim_generation));
    assert_eq!(
        ownership["lend"],
        json!(null),
        "the lend's effect never landed"
    );
    let quarantine = &ownership["quarantined"];
    assert_eq!(
        quarantine["reason"]["kind"],
        json!("unresolved_lend_intent")
    );
    assert_eq!(quarantine["reason"]["operation_id"], json!("op-lend-crash"));
    assert_eq!(quarantine["reason"]["borrower_cell_id"], json!(CELL_A));

    // The in-doubt retry never re-executes (the Terminal class).
    let (status, in_doubt) = rig.lend("op-lend-crash", CELL_A, claim_generation).await;
    assert_refusal(&in_doubt, status, "OPERATION_IN_DOUBT");

    // A fresh lend is refused while the quarantine holds, as is the
    // claiming layer's release.
    let (status, refusal) = rig.lend("op-lend-fresh", CELL_A, claim_generation).await;
    assert_refusal(&refusal, status, "INVALID_STATE");
    assert!(
        refusal["message"]
            .as_str()
            .is_some_and(|m| m.contains("quarantined")),
        "the refusal must name the quarantine: {refusal}"
    );
    let (status, refusal) = rig.release("op-release-1").await;
    assert_refusal(&refusal, status, "INVALID_STATE");

    // The explicit exit reconciles by what the record proves: the
    // lend never landed, so the device is unlent and operable.
    let (status, resolved) = rig.resolve("op-resolve-1", OPERATOR_TOKEN).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the reconciliation must succeed: {resolved}"
    );
    assert_eq!(resolved["lend"], json!(null));
    assert_eq!(resolved["quarantined"], json!(null));

    // The fresh lend completes at the generation the crashed lend
    // never consumed.
    let (status, lent) = rig.lend("op-lend-after", CELL_A, claim_generation).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(lent["lend"]["lend_generation"], json!(1));

    // Resolving a device that is not quarantined is the typed
    // refusal.
    let (status, refusal) = rig.resolve("op-resolve-2", OPERATOR_TOKEN).await;
    assert_refusal(&refusal, status, "INVALID_STATE");
}

/// **Invariant 6 (the reclaim window)**: a crash between the
/// reclaim's journal and its effect leaves the device **still lent
/// and quarantined**; a journaled foreign observation is honored by
/// the reconciliation — the verdict was recorded before the crash,
/// and ignoring it would risk reusing foreign residue.
#[tokio::test]
async fn invariant_6_crash_between_the_reclaim_journal_and_its_effect_fails_closed() {
    install_panic_filter();
    let mut rig = LendingRig::new();
    rig.start();
    let claim_generation = claimed(&rig).await;
    let lend_generation = lent(&rig, "op-lend-1", claim_generation).await;

    // The borrower releases, then the reclaim dies between its
    // journal and its effect.
    let world = rig.world();
    world
        .model_borrower_takes(&device(), &cell(CELL_A))
        .expect("model borrower takes");
    world
        .model_borrower_release(&device())
        .expect("model borrower release");
    rig.store_crash
        .arm(STORE_DEVICE_OWNERSHIP, StoreSavePoint::AfterRename);
    let err = fire(
        rig.app().clone(),
        Method::POST,
        &format!("/v2/admin/devices/{DEVICE}/reclaim"),
        &json!({
            "api_version": "volvisor.volume.v2",
            "operation_id": "op-reclaim-crash",
            "expected_lend_generation": lend_generation,
        }),
    )
    .await
    .expect_err("the reclaim must die to the armed kill");
    assert_injected_kill(err);

    // The restart: still lent, quarantined on the unresolved intent.
    rig.start();
    let (status, ownership) = rig.ownership().await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        ownership["lend"]["lend_generation"],
        json!(lend_generation),
        "the reclaim's effect never landed"
    );
    assert_eq!(
        ownership["quarantined"]["reason"]["kind"],
        json!("unresolved_reclaim_intent")
    );
    assert_eq!(
        ownership["quarantined"]["reason"]["operation_id"],
        json!("op-reclaim-crash")
    );

    // The in-doubt retry never re-executes; a fresh reclaim waits
    // for the quarantine to lift.
    let (status, in_doubt) = rig.reclaim("op-reclaim-crash", lend_generation).await;
    assert_refusal(&in_doubt, status, "OPERATION_IN_DOUBT");
    let (status, refusal) = rig.reclaim("op-reclaim-fresh", lend_generation).await;
    assert_refusal(&refusal, status, "INVALID_STATE");

    // The reconciliation keeps the lend live (the effect never
    // landed), and the reclaim then completes over the released
    // surface of the restarted world.
    let (status, resolved) = rig.resolve("op-resolve-1", OPERATOR_TOKEN).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the reconciliation must succeed: {resolved}"
    );
    assert!(
        resolved["lend"].is_object(),
        "the reclaim resolves to still-lent"
    );
    assert_eq!(resolved["quarantined"], json!(null));
    let (status, ack) = rig.reclaim("op-reclaim-after", lend_generation).await;
    assert_eq!(status, StatusCode::OK, "the reclaim must complete: {ack}");
    assert_eq!(ack["observation"]["observed"]["kind"], json!("released"));

    // The foreign-observation variant: the reclaim died after the
    // residue check recorded its verdict, so the reconciliation
    // honors it into the foreign-residue quarantine — and only the
    // operator's second, explicit resolution ends the tenure.
    let lend_generation = lent(&rig, "op-lend-2", claim_generation).await;
    let world = rig.world();
    world
        .model_foreign_residue(&device(), "foreign zfs superblock")
        .expect("model foreign residue");
    rig.store_crash
        .arm(STORE_DEVICE_OWNERSHIP, StoreSavePoint::AfterRename);
    let err = fire(
        rig.app().clone(),
        Method::POST,
        &format!("/v2/admin/devices/{DEVICE}/reclaim"),
        &json!({
            "api_version": "volvisor.volume.v2",
            "operation_id": "op-reclaim-foreign-crash",
            "expected_lend_generation": lend_generation,
        }),
    )
    .await
    .expect_err("the reclaim must die to the armed kill");
    assert_injected_kill(err);

    rig.start();
    let (status, resolved) = rig.resolve("op-resolve-2", OPERATOR_TOKEN).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        resolved["quarantined"]["reason"]["kind"],
        json!("foreign_residue"),
        "the journaled foreign verdict is honored"
    );
    assert_eq!(
        resolved["quarantined"]["reason"]["details"],
        json!("foreign zfs superblock")
    );
    assert!(
        resolved["lend"].is_object(),
        "the tenure has not ended by reconciliation alone"
    );
    let (status, accepted) = rig.resolve("op-resolve-3", OPERATOR_TOKEN).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the operator's acceptance must succeed: {accepted}"
    );
    assert_eq!(accepted["lend"], json!(null));
    assert_eq!(accepted["quarantined"], json!(null));
}

/// **Invariant 6 (the API journal window)**: a crash after the API
/// journal's intent append but before the device layer is touched
/// leaves the device **unlent and unquarantined** — nothing was
/// mutated, so there is nothing to reconcile — and the retry is the
/// in-doubt refusal: the outcome is never guessed from the device's
/// observed state.
#[tokio::test]
async fn invariant_6_an_api_journal_intent_without_outcome_is_in_doubt_never_guessed() {
    install_panic_filter();
    let mut rig = LendingRig::new();
    rig.start();
    let claim_generation = claimed(&rig).await;

    // Arm the API journal's after-intent point: the intent is
    // durable, the provider was never called.
    rig.crash.arm(ops::OP_LEND_DEVICE, CrashPoint::AfterIntent);
    let err = fire(
        rig.app().clone(),
        Method::POST,
        &format!("/v2/admin/devices/{DEVICE}/lend"),
        &json!({
            "api_version": "volvisor.volume.v2",
            "operation_id": "op-lend-api-crash",
            "borrower_cell_id": CELL_A,
            "expected_claim_generation": claim_generation,
        }),
    )
    .await
    .expect_err("the lend must die to the armed kill");
    assert_injected_kill(err);

    // Nothing landed: unlent, unquarantined — the fail-closed answer
    // for a window that mutated nothing.
    rig.start();
    let (status, ownership) = rig.ownership().await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ownership["lend"], json!(null));
    assert_eq!(ownership["quarantined"], json!(null));

    // The in-doubt retry never re-executes — the device's clean
    // observation does not "resolve" the intent.
    let (status, in_doubt) = rig
        .lend("op-lend-api-crash", CELL_A, claim_generation)
        .await;
    assert_refusal(&in_doubt, status, "OPERATION_IN_DOUBT");

    // A fresh operation is free to proceed (the previous one is
    // dead, not half-applied).
    let (status, lent) = rig.lend("op-lend-fresh", CELL_A, claim_generation).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the fresh lend must succeed: {lent}"
    );
    assert_eq!(lent["lend"]["lend_generation"], json!(1));
}

/// **Invariant 6 (the unreadable record)**: an ownership record that
/// cannot be read leaves the device quarantined with the claim state
/// unknown — surfaced honestly in `DeviceOwnership`, refused by
/// every mutation, failed closed by discovery, and with **no
/// automated exit**: the refusal names the manual remediation. The
/// corruption survives restarts.
#[tokio::test]
async fn invariant_6_an_unreadable_ownership_record_is_quarantined_with_no_exit() {
    let mut rig = LendingRig::new();
    rig.start();
    let claim_generation = claimed(&rig).await;
    let lend_generation = lent(&rig, "op-lend-1", claim_generation).await;

    // The corruption (the testkit's model of an unparseable record).
    rig.world()
        .inject_unreadable_record(&device())
        .expect("inject the unreadable record");

    // The honest answer: unknown claim, not lent, quarantined
    // unreadable — never an inferred state from the fields the
    // record happens to carry.
    let (status, ownership) = rig.ownership().await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ownership["claimed_by"], json!("unknown"));
    assert_eq!(ownership["claim_generation"], json!(0));
    assert_eq!(ownership["lend"], json!(null));
    assert_eq!(
        ownership["quarantined"]["reason"]["kind"],
        json!("unreadable_record")
    );

    // Every mutation refuses, and discovery fails closed rather
    // than answer for a record it does not trust.
    let (status, refusal) = rig.lend("op-lend-2", CELL_A, claim_generation).await;
    assert_refusal(&refusal, status, "INVALID_STATE");
    let (status, refusal) = rig.reclaim("op-reclaim-1", lend_generation).await;
    assert_refusal(&refusal, status, "INVALID_STATE");
    let (status, refusal) = rig.release("op-release-1").await;
    assert_refusal(&refusal, status, "INVALID_STATE");
    let (status, refusal) = rig.discover().await;
    assert_refusal(&refusal, status, "INVALID_STATE");

    // No automated exit: the resolve refuses and names the manual
    // remediation.
    let (status, refusal) = rig.resolve("op-resolve-1", OPERATOR_TOKEN).await;
    assert_refusal(&refusal, status, "INVALID_STATE");
    assert!(
        refusal["message"]
            .as_str()
            .is_some_and(|m| m.contains("no automated exit")),
        "the refusal must name the manual remediation: {refusal}"
    );

    // The corruption is durable: the restart sees the same
    // quarantined-unreadable answer.
    rig.start();
    let (status, ownership) = rig.ownership().await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ownership["claimed_by"], json!("unknown"));
    assert_eq!(
        ownership["quarantined"]["reason"]["kind"],
        json!("unreadable_record")
    );
}

// ------------------------------------------------------------------
// The seam's error path
// ------------------------------------------------------------------

/// The observation seam's fail-closed path: when the surface cannot
/// be observed, the reclaim refuses with **no state claims** —
/// nothing journaled into the record, no pending intent, the lend
/// untouched — and the device is operable once the observation works
/// again.
#[tokio::test]
async fn an_observation_failure_refuses_without_writing_anything() {
    let mut rig = LendingRig::new();
    rig.start();
    let claim_generation = claimed(&rig).await;
    let lend_generation = lent(&rig, "op-lend-1", claim_generation).await;

    let world = rig.world();
    world
        .model_surface_unreadable(&device())
        .expect("model the unreadable surface");
    let (status, refusal) = rig.reclaim("op-reclaim-1", lend_generation).await;
    assert_refusal(&refusal, status, "INTERNAL");

    // Nothing was written: the lend intact, no quarantine derived
    // from a phantom intent.
    let (status, ownership) = rig.ownership().await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ownership["lend"]["lend_generation"], json!(lend_generation));
    assert_eq!(ownership["quarantined"], json!(null));

    // With the surface observable again, a fresh reclaim completes.
    world
        .model_borrower_release(&device())
        .expect("model release");
    let (status, ack) = rig.reclaim("op-reclaim-2", lend_generation).await;
    assert_eq!(status, StatusCode::OK, "the reclaim must complete: {ack}");
    assert_eq!(ack["observation"]["observed"]["kind"], json!("released"));
}

// ------------------------------------------------------------------
// The optional-surface contract
// ------------------------------------------------------------------

/// The lending routes on a provider without a lending surface answer
/// the typed 404 — the optional-surface pattern, exactly like the
/// admin and adoption surfaces (and the volvisord runtime until
/// P9-B's real leg wires the surface in).
#[tokio::test]
async fn the_lending_routes_answer_the_typed_404_without_a_surface() {
    let dir = tempfile::tempdir().expect("temporary journal directory");
    let journal = Journal::open(dir.path()).expect("journal open");
    let provider = Arc::new(FakeProvider::new());
    let admin: Arc<dyn AdminSurface> = provider.clone();
    let state = Arc::new(AppState::new(
        provider.clone(),
        Some(admin),
        journal,
        Some(ADMIN_TOKEN.to_owned()),
    ));
    let app = app(&state);

    for (method, uri, body) in [
        (
            Method::POST,
            format!("/v2/admin/devices/{DEVICE}/lend"),
            json!({
                "api_version": "volvisor.volume.v2",
                "operation_id": "op-lend-1",
                "borrower_cell_id": CELL_A,
                "expected_claim_generation": 1,
            }),
        ),
        (
            Method::POST,
            format!("/v2/admin/devices/{DEVICE}/reclaim"),
            json!({
                "api_version": "volvisor.volume.v2",
                "operation_id": "op-reclaim-1",
                "expected_lend_generation": 1,
            }),
        ),
        (
            Method::POST,
            format!("/v2/admin/devices/{DEVICE}/resolve-quarantine"),
            json!({
                "api_version": "volvisor.volume.v2",
                "operation_id": "op-resolve-1",
                "operator_authorization": OPERATOR_TOKEN,
            }),
        ),
        (
            Method::GET,
            format!("/v2/admin/devices/{DEVICE}/ownership"),
            json!({}),
        ),
    ] {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::AUTHORIZATION, format!("Bearer {ADMIN_TOKEN}"))
            .body(Body::from(body.to_string()))
            .expect("build request");
        let response = app.clone().oneshot(request).await.expect("router call");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read response body");
        let body: Value = serde_json::from_slice(&bytes).expect("JSON error body");
        assert_eq!(body["code"], json!("NOT_FOUND"));
        assert!(
            body["message"]
                .as_str()
                .is_some_and(|m| m.contains("lending surface not available")),
            "the 404 must name the missing surface: {body}"
        );
    }
}
