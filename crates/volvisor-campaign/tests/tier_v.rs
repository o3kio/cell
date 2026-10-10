//! The Tier V scenario rows (ADR-0010, plan stage P8-A): the four
//! bounded real-VMM scenarios, driven through
//! [`volvisor_campaign::tier_v::drive`] — the skipped gate emits
//! each scenario's skip record (the default suite asserts them;
//! the CI runners never carry KVM or Cloud Hypervisor), and
//! `VOLVISOR_TEST_VMM=1` runs the bodies against the real pair
//! this host provides.
//!
//! The composition in every body: the REAL VMM processes
//! ([`volvisor_campaign::tier_v::RealVmmRig`]) stand behind the
//! campaign rig's [`VmmController`] seam — each daemon drives a
//! [`volvisor_campaign::tier_v::MirroredVmm`] (a real `ChRemoteVmm`
//! whose observed state transitions mirror into the rig's fake
//! view, so the device model below the seam follows the real VMM's
//! acts) — while the storage below the seam stays simulated
//! (ADR-0010's explicit allowance, stated in every record's
//! `components`). What each scenario verifies is the seam: the
//! real pause/snapshot/destroy/restore/resume state machine, the
//! migration protocol's VMM acts at real timing, the write-trace
//! oracle's boundary at a REAL pause, and the P6 grow
//! notification's typed contract against a real binary.
//!
//! The rows are SERIALIZED (the [`REAL_SERIES`] lock): each body
//! boots real guests (512 MiB of real memory each, two VMM
//! processes per migration scenario) and owns the
//! `/dev/drbd{SEED_MINOR}` device files — real resources with real
//! bounds, deliberately not inheriting the fake tier's parallelism.

// Test-kit code: invariant assertions may use expect/unwrap, and
// the scenario bodies exceed the line budget by design.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::too_many_lines)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use volvisor_api::AppState;
use volvisor_api::crash::CrashPoint;
use volvisor_api::op_kinds;
use volvisor_api::router;
use volvisor_campaign::evidence::{Evidence, LogSources, oracle_value};
use volvisor_campaign::oracle::{
    AckedWrite, StopReason, WRITER_ID, WriterHandle, barrier_timestamp, boundary_skew,
    verify_against,
};
use volvisor_campaign::rig::{
    GIB, Rig, body_json, campaign_rig_volumes_with_vmm, get_volume, post_prepare, post_transfer,
    role_of, state_name, witness_view,
};
use volvisor_campaign::tier_v::{self, RealVmmEnv, RealVmmRig};
use volvisor_drbd::report::Role;
use volvisor_drbd_testkit::{PEER_NODE, SEED_MINOR, spawn_peer_transport};
use volvisor_journal::Journal;
use volvisor_lvm::LvmProvider;
use volvisor_provider::vmm::FakeVmm;
use volvisor_provider::{
    CommandOutput, CommandRunner, DiskMapping, FakeRunner, GrowAttachmentFacts,
    GrowNotificationEngine, GrowNotificationStore, GrowNotifier, RealRunner, VmState,
    VmmController, VmmVersionGate,
};
use volvisor_types::id::VolumeId;
use volvisor_types::{ApiErrorCode, MigrationId};

// ------------------------------------------------- the serialization lock

/// The real-VMM scenarios run one at a time: each body owns real
/// guest memory (512 MiB per VM, two processes per migration
/// scenario) and the `/dev/drbd{SEED_MINOR}` device files. Cargo
/// runs test BINARIES sequentially, so only this binary's
/// intra-binary parallelism needs the lock.
static REAL_SERIES: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();

/// Serialize one body under [`REAL_SERIES`].
async fn serialized<T>(body: impl std::future::Future<Output = T>) -> T {
    let lock = REAL_SERIES.get_or_init(|| tokio::sync::Mutex::new(()));
    let _guard = lock.lock().await;
    body.await
}

static SEQ: AtomicU64 = AtomicU64::new(1);

/// The unique id suffix (stable ordering across parallel scenarios).
fn next_id() -> u64 {
    SEQ.fetch_add(1, Ordering::SeqCst)
}

/// The real tier's polling budget: a real VMM's snapshot and
/// restore move 512 MiB of real memory through real subprocesses —
/// the Tier S 15-second bound is a fake-world affordance the real
/// tier does not inherit. Bounded polling throughout, never a bare
/// sleep.
const REAL_POLL_BOUND: Duration = Duration::from_secs(90);
const POLL_STEP: Duration = Duration::from_millis(50);

/// Bounded wait for a migration to reach `want_state` (the real
/// tier's budget).
async fn await_state(addr: SocketAddr, mig: &str, want_state: &str) -> Value {
    let deadline = tokio::time::Instant::now() + REAL_POLL_BOUND;
    loop {
        let (status, body) = volvisor_campaign::rig::get_migration(addr, mig)
            .await
            .served("observe migration");
        assert_eq!(status, 200, "observe migration: {body}");
        let summary = body_json(&body);
        if state_name(&summary) == want_state {
            return summary;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "migration {mig} never reached {want_state} within {}s (last: {})",
            REAL_POLL_BOUND.as_secs(),
            serde_json::to_string_pretty(&summary).unwrap_or_default()
        );
        tokio::time::sleep(POLL_STEP).await;
    }
}

/// Bounded wait for a killed daemon to actually die (the abort
/// completes asynchronously from the killer's view).
async fn await_kill(killed: impl Fn() -> bool, what: &str) {
    let deadline = tokio::time::Instant::now() + REAL_POLL_BOUND;
    while !killed() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the {what} kill never landed within {}s",
            REAL_POLL_BOUND.as_secs()
        );
        tokio::time::sleep(POLL_STEP).await;
    }
}

// ---------------------------------------------------- shared drive helpers

/// The prepare act (admin route; 201).
async fn prepare(rig: &Rig, mig: &str) {
    let (status, body) = post_prepare(
        rig.a.addr,
        &json!({
            "migration_id": mig,
            "vm_id": rig.vm,
            "target_host": PEER_NODE,
            "volume_ids": [rig.volume],
            "expected_generations": [2],
        }),
    )
    .await
    .served("prepare");
    assert_eq!(status, 201, "prepare: {body}");
}

/// The transfer act (admin route; the 202 the drive spawns under).
async fn transfer(rig: &Rig, mig: &str) -> (u16, String) {
    post_transfer(rig.a.addr, mig).await.served("transfer")
}

/// The G5 check for COMPLETE paths (rule 5): the migration's
/// barrier is recorded at the witness, NOT voided, with every
/// attested fact true — and the source was never resumed over it
/// (the MIRRORED view's VM destroyed, resource Secondary). Returns
/// the invariant verdict string.
async fn assert_g5_complete(rig: &Rig, mig: &str) -> String {
    let view = witness_view(&rig.witness, &rig.volume_id()).await;
    let migration = MigrationId::new(mig).expect("valid migration id");
    let matching: Vec<_> = view
        .barriers
        .iter()
        .filter(|barrier| barrier.migration_id.as_ref() == Some(&migration))
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "exactly one barrier recorded for {mig}: {:?}",
        view.barriers
    );
    assert!(
        !matching[0].voided,
        "a completed migration's barrier is never voided"
    );
    assert!(
        matching[0].attestation.all_true(),
        "the barrier's attested facts all hold"
    );
    assert_eq!(
        rig.vmm_a.vm_state(&rig.vm).expect("source VM state"),
        VmState::Absent,
        "the source VM is destroyed (never resumed)"
    );
    assert_eq!(
        role_of(&rig.world_a, &rig.resource()),
        Role::Secondary,
        "the source resource is demoted"
    );
    "pass: barrier non-voided and all-true; source never resumed".to_owned()
}

/// D6a over the operator surface: the provider's inspect answers
/// the promoted epoch and holder.
async fn assert_d6a(rig: &Rig, addr: SocketAddr, epoch: u64, holder: &str) -> String {
    let (status, body) = get_volume(addr, &rig.volume)
        .await
        .served("provider inspect");
    assert_eq!(status, 200, "provider inspect: {body}");
    let value = body_json(&body);
    assert_eq!(
        value["authority"]["epoch"],
        json!(epoch),
        "provider inspect authority epoch: {body}"
    );
    assert_eq!(
        value["authority"]["holder"],
        json!(holder),
        "provider inspect authority holder: {body}"
    );
    format!("pass: authority epoch {epoch}, holder {holder}")
}

/// Emit the scenario's evidence record: the oracle section from the
/// byte-level verdict and boundary (§2.3's rule-1 cross-check
/// against the coordinator's barrier timestamp), the log capture,
/// and the report refresh. Returns the record path.
fn emit(
    rig: &Rig,
    mut evidence: Evidence,
    acked: &[AckedWrite],
    boundary: Option<(u64, StopReason)>,
    verified_side: &str,
    summary: Option<&Value>,
) -> PathBuf {
    let verdict_source = verify_against(&rig.world_a, SEED_MINOR, acked, WRITER_ID);
    let verdict_peer = verify_against(&rig.world_b, SEED_MINOR, acked, WRITER_ID);
    let (verified, tail) = if verified_side == "destination" {
        (verdict_peer.present, verdict_source.tail())
    } else {
        (verdict_source.present, verdict_peer.tail())
    };
    let (boundary_seq, stop_reason) =
        boundary.map_or((None, "nothing-acked".to_owned()), |(seq, reason)| {
            (
                Some(seq),
                match reason {
                    StopReason::Paused => "paused".to_owned(),
                    StopReason::Refused(detail) => format!("refused: {detail}"),
                    StopReason::StoppedByRig => "stopped-by-rig".to_owned(),
                },
            )
        });
    let barrier_at = summary.and_then(barrier_timestamp);
    let skew = barrier_at.and_then(|at| boundary_skew(acked, at));
    evidence.oracle(oracle_value(
        acked.len() as u64,
        verified,
        verdict_peer.corrupted.max(verdict_source.corrupted),
        tail,
        verified_side,
        boundary_seq,
        "data-path",
        &stop_reason,
        barrier_at,
        skew,
    ));
    evidence.finish(&LogSources {
        a_journal: &rig.a.core.journal_dir,
        b_journal: &rig.b.core.journal_dir,
        witness: &rig.witness.dir,
    })
}

/// Capture the destination's wired fake view at rig construction
/// (the mirror-coherence assertion surface: after a completed
/// migration the view the writer model read must agree with the
/// real VMM's answer). The slot hands back the factory that both
/// stores the view and builds the real mirrored controllers.
fn capturing_factory(
    real: &Arc<RealVmmRig>,
    target_view: Arc<Mutex<Option<Arc<FakeVmm>>>>,
) -> volvisor_campaign::rig::VmmControllerFactory {
    let inner = real.factory();
    Arc::new(move |view_a, view_b| {
        *target_view.lock().expect("the view slot") = Some(Arc::clone(&view_b));
        inner(view_a, view_b)
    })
}

// ------------------------------------------------- scenario (a): the row

/// Scenario (a): the migration happy path end-to-end against a real
/// VMM — the full mobility protocol through the public routes, with
/// every VMM act landing on a live cloud-hypervisor and every state
/// claim observed over the api sockets.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tier_v_migration_happy_path() {
    let record = tier_v::drive(tier_v::scenario("migration-happy-path"), |env| async move {
        serialized(real_migration_happy_path(env)).await
    })
    .await;
    assert!(
        record.is_file(),
        "the tier-v/migration-happy-path record landed: {record:?}"
    );
}

async fn real_migration_happy_path(env: RealVmmEnv) -> PathBuf {
    let seq = next_id();
    let vm = format!("vm-tv1-{seq}");
    let volume = format!("vol-tv1-{seq}");
    // The real rig first (dropped LAST — after the campaign rig): it
    // owns the VMM processes and the /dev/drbd{SEED_MINOR} file.
    let real = Arc::new(
        RealVmmRig::launch_pair(env.clone(), &vm, &[SEED_MINOR], GIB)
            .await
            .expect("the claimed environment backs the real-VMM launch"),
    );
    let target_view: Arc<Mutex<Option<Arc<FakeVmm>>>> = Arc::new(Mutex::new(None));
    let rig = campaign_rig_volumes_with_vmm(
        &vm,
        &[&volume],
        capturing_factory(&real, Arc::clone(&target_view)),
    )
    .await;
    // The consumer's act: boot the real guest (the rig is the
    // consumer — volvisor never launches VMMs).
    real.boot_source_vm()
        .await
        .expect("the real guest reaches the busybox shell");

    // The pre-drive observation: the real VM is genuinely running
    // over the source's api socket.
    assert_eq!(
        real.source_vmm().state(&vm).expect("the source VM state"),
        VmState::Running,
        "the real guest is running before the drive"
    );

    let mut evidence = Evidence::new_tier_v("tier-v/migration-happy-path");
    evidence.components(env.components_value());

    // The drive: the public routes only (prepare, transfer, poll).
    prepare(&rig, "mig-tv1").await;
    let (status, body) = transfer(&rig, "mig-tv1").await;
    assert_eq!(status, 202, "the transfer spawns the drive: {body}");
    let summary = await_state(rig.a.addr, "mig-tv1", "complete").await;

    // The REAL VMM states after completion, observed over the api
    // sockets: the source VM was destroyed (the device release the
    // demote gates on), the destination VM was restored and resumed.
    assert_eq!(
        real.source_vmm().state(&vm).expect("the source VM state"),
        VmState::Absent,
        "the real source VM is destroyed (never resumed)"
    );
    assert_eq!(
        real.target_vmm()
            .state(&vm)
            .expect("the destination VM state"),
        VmState::Running,
        "the real destination VM is restored and resumed"
    );
    // The MIRROR's coherence: the fake view the rig's device model
    // read followed the real acts (a diverged view would have held
    // the device open and blocked the demote — the migration could
    // not have completed).
    assert_eq!(
        rig.vmm_a.vm_state(&vm).expect("the mirrored source view"),
        VmState::Absent,
        "the mirrored view agrees with the real destroy"
    );
    let mirrored_target = target_view
        .lock()
        .expect("the view slot")
        .clone()
        .expect("the factory captured the destination view");
    assert_eq!(
        mirrored_target
            .vm_state(&vm)
            .expect("the mirrored target view"),
        VmState::Running,
        "the mirrored view agrees with the real restore/resume"
    );

    // The snapshot artifacts are REAL: the coordinator's snapshot
    // directory holds cloud-hypervisor's actual files.
    let snapshot_dir = rig.snapshot_root.join(&vm);
    for artifact in ["config.json", "memory-ranges", "state.json"] {
        assert!(
            snapshot_dir.join(artifact).is_file(),
            "the real snapshot artifact {artifact} exists at {}",
            snapshot_dir.display()
        );
    }

    // The protocol invariants over the operator surfaces.
    evidence.invariant(
        "g5_no_resume_over_barrier",
        &assert_g5_complete(&rig, "mig-tv1").await,
    );
    evidence.invariant(
        "d6a_provider_inspect",
        &assert_d6a(&rig, rig.b.addr, 2, PEER_NODE).await,
    );
    evidence.invariant(
        "real_vmm_state_claims_observed",
        "pass: every VM state claim is an api-socket observation (source destroyed, \
         destination resumed), never a command's exit status",
    );
    evidence.outcome(
        "complete: the full mobility protocol (prepare → transfer → cut → grant → restore → \
         resume) \
         landed on a real cloud-hypervisor pair — every VM state claim an api-socket \
         observation (the binary stamps are this record's components)",
    );
    emit(&rig, evidence, &[], None, "destination", Some(&summary))
}

// ------------------------------------------------- scenario (b): the row

/// Scenario (b): the pause/snapshot kill window — the destination
/// daemon dies inside the cut's restore act (the real restore in
/// flight), and the recovery re-drives the real VMM acts forward.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tier_v_kill_window_mid_restore() {
    let record = tier_v::drive(
        tier_v::scenario("kill-window-mid-restore"),
        |env| async move { serialized(real_kill_window_mid_restore(env)).await },
    )
    .await;
    assert!(
        record.is_file(),
        "the tier-v/kill-window-mid-restore record landed: {record:?}"
    );
}

async fn real_kill_window_mid_restore(env: RealVmmEnv) -> PathBuf {
    let seq = next_id();
    let vm = format!("vm-tv2-{seq}");
    let volume = format!("vol-tv2-{seq}");
    let real = Arc::new(
        RealVmmRig::launch_pair(env.clone(), &vm, &[SEED_MINOR], GIB)
            .await
            .expect("the claimed environment backs the real-VMM launch"),
    );
    let target_view: Arc<Mutex<Option<Arc<FakeVmm>>>> = Arc::new(Mutex::new(None));
    let mut rig = campaign_rig_volumes_with_vmm(
        &vm,
        &[&volume],
        capturing_factory(&real, Arc::clone(&target_view)),
    )
    .await;
    real.boot_source_vm()
        .await
        .expect("the real guest reaches the busybox shell");

    let mut evidence = Evidence::new_tier_v("tier-v/kill-window-mid-restore");
    evidence.components(env.components_value());
    evidence.fault(
        volvisor_campaign::matrix::Hook::Journal(CrashPoint::BeforeOutcome).fault_kind(),
        "journal peer_restore_vm before outcome",
    );

    // Arm the destination's kill inside the cut's restore act, then
    // drive: the 202 spawns the migration drive whose real pause,
    // snapshot and destroy complete before the peer grant's restore
    // is interrupted by the kill.
    rig.b
        .core
        .crash
        .arm(op_kinds::OP_PEER_RESTORE_VM, CrashPoint::BeforeOutcome);
    prepare(&rig, "mig-tv2").await;
    let (status, body) = transfer(&rig, "mig-tv2").await;
    assert_eq!(status, 202, "the transfer spawns the drive: {body}");
    await_kill(|| rig.b.is_killed(), "peer restore").await;

    // The kill leg's REAL observations: the cut was durable on the
    // source (the real VM destroyed before the peer grant — the
    // protocol's order), and the destination's real VM is wherever
    // the interrupted restore left it (Absent, or restored-Paused,
    // or resumed-Running if the blocking act completed before the
    // abort) — recorded, never assumed.
    assert_eq!(
        real.source_vmm().state(&vm).expect("the source VM state"),
        VmState::Absent,
        "the cut is durable: the real source VM was destroyed before the kill"
    );
    let target_at_kill = real
        .target_vmm()
        .state(&vm)
        .expect("the destination VM state");
    assert!(
        matches!(
            target_at_kill,
            VmState::Absent | VmState::Paused | VmState::Running
        ),
        "the interrupted restore leaves a legal real state: {target_at_kill}"
    );

    // The recorded clustering (the Tier S rows' operator order):
    // restart the destination, then the source — the startup pass
    // re-drives the durable cut forward over the REAL VMMs (the
    // re-drive's destroy is a no-op on the already-absent source
    // VM; the destination's restore resolves whatever the
    // interrupted attempt left).
    rig.b.restart().await;
    rig.a.restart().await;
    let summary = await_state(rig.a.addr, "mig-tv2", "complete").await;

    // The recovered REAL states: the re-drive resumed the restored
    // destination VM.
    assert_eq!(
        real.source_vmm().state(&vm).expect("the source VM state"),
        VmState::Absent,
        "the source stays destroyed after the re-drive"
    );
    assert_eq!(
        real.target_vmm()
            .state(&vm)
            .expect("the destination VM state"),
        VmState::Running,
        "the re-driven restore+resume leaves the real destination VM running"
    );
    assert_eq!(
        rig.vmm_a.vm_state(&vm).expect("the mirrored source view"),
        VmState::Absent,
        "the mirrored view agrees with the real destroy"
    );

    // The protocol invariants: the barrier was never voided and the
    // re-driven grant minted no extra epoch (exactly 2 — a re-drive
    // that re-executed the grant would mint 3).
    evidence.invariant(
        "g5_no_resume_over_barrier",
        &assert_g5_complete(&rig, "mig-tv2").await,
    );
    evidence.invariant(
        "d6a_no_double_grant",
        &assert_d6a(&rig, rig.b.addr, 2, PEER_NODE).await,
    );
    evidence.invariant(
        "kill_leg_real_state_recorded",
        &format!(
            "pass: the interrupted restore left the destination VM {target_at_kill} \
             (observed over the api socket at the kill leg); the source VM was \
             already destroyed — the cut's ordering held over the real VMM"
        ),
    );
    evidence.outcome(
        "recovered: the destination kill inside the real restore re-drove to COMPLETE \
         through the startup pass — the re-drive's acts landed on the real VMMs and the \
         kill leg's mid-flight state was observed over the api sockets, never assumed",
    );
    emit(&rig, evidence, &[], None, "destination", Some(&summary))
}

// ------------------------------------------------- scenario (c): the row

/// Scenario (c): snapshot/restore divergence detection over a real
/// VMM — the write-trace oracle's boundary at a REAL pause, plus
/// the adapter's typed refusals of a divergent snapshot.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tier_v_snapshot_restore_divergence() {
    let record = tier_v::drive(
        tier_v::scenario("snapshot-restore-divergence"),
        |env| async move { serialized(real_snapshot_restore_divergence(env)).await },
    )
    .await;
    assert!(
        record.is_file(),
        "the tier-v/snapshot-restore-divergence record landed: {record:?}"
    );
}

async fn real_snapshot_restore_divergence(env: RealVmmEnv) -> PathBuf {
    let seq = next_id();
    let vm = format!("vm-tv3-{seq}");
    let volume = format!("vol-tv3-{seq}");
    let real = Arc::new(
        RealVmmRig::launch_pair(env.clone(), &vm, &[SEED_MINOR], GIB)
            .await
            .expect("the claimed environment backs the real-VMM launch"),
    );
    let target_view: Arc<Mutex<Option<Arc<FakeVmm>>>> = Arc::new(Mutex::new(None));
    let rig = campaign_rig_volumes_with_vmm(
        &vm,
        &[&volume],
        capturing_factory(&real, Arc::clone(&target_view)),
    )
    .await;
    real.boot_source_vm()
        .await
        .expect("the real guest reaches the busybox shell");

    let mut evidence = Evidence::new_tier_v("tier-v/snapshot-restore-divergence");
    evidence.components(env.components_value());

    // The live writer and the steady-state transport (§2.2): the
    // writer's I/O model polls the MIRRORED view, so its boundary
    // lands at the REAL pause the migration drives.
    let writer = WriterHandle::start(&rig.world_a, &rig.vmm_a, &vm, SEED_MINOR, &rig.stamp_clock);
    let transport = spawn_peer_transport(&rig.world_a, SEED_MINOR, Duration::from_millis(1));
    tokio::time::sleep(Duration::from_millis(80)).await;

    prepare(&rig, "mig-tv3").await;
    let (status, body) = transfer(&rig, "mig-tv3").await;
    assert_eq!(status, 202, "the transfer spawns the drive: {body}");
    let summary = await_state(rig.a.addr, "mig-tv3", "complete").await;
    let (acked, boundary) = writer.join().await;
    transport.join();
    assert!(
        !acked.is_empty(),
        "the live writer acknowledged writes before the cut (boundary: {boundary:?})"
    );

    // The boundary is the data path's own answer at the REAL pause
    // (the mirrored view the writer polls followed the real pause),
    // never the rig's stop.
    let (_, stop_reason) = boundary
        .clone()
        .expect("the writer's I/O ended with the cut");
    assert!(
        matches!(stop_reason, StopReason::Paused | StopReason::Refused(_)),
        "the boundary is the real pause (or the suspension refusal it caused): \
         {stop_reason:?}"
    );

    // The oracle across the real pair: every acknowledged write is
    // present and crc-correct at the destination; the tail is
    // honestly quantified.
    let destination = verify_against(&rig.world_b, SEED_MINOR, &acked, WRITER_ID);
    assert!(
        destination.prefix_intact(),
        "the acknowledged prefix is intact at the destination ({} present, {} corrupted)",
        destination.present,
        destination.corrupted
    );
    assert_eq!(destination.corrupted, 0, "no corrupted blocks");
    assert_eq!(
        real.target_vmm()
            .state(&vm)
            .expect("the destination VM state"),
        VmState::Running,
        "the real destination VM is resumed"
    );

    // ---- the adapter's typed refusals against the real VMM ----
    let snapshot_dir = rig.snapshot_root.join(&vm);
    let target = real.target_vmm();
    // (1) A non-empty VMM refuses the restore (the real api-socket
    // observation — the refusal precedes any disk rewrite).
    let error = target
        .restore(&vm, &snapshot_dir, &[])
        .expect_err("a non-empty real VMM refuses the restore");
    assert_eq!(
        error.code,
        ApiErrorCode::InvalidState,
        "the non-empty refusal is typed: {error}"
    );
    // (2) A snapshot whose mapping matches no config disk is never
    // accepted: destroy the restored VM first (the coordinator's
    // documented order), then attempt the divergent restore.
    target.destroy(&vm).expect("the real destroy");
    let error = target
        .restore(
            &vm,
            &snapshot_dir,
            &[DiskMapping {
                declared_path: "/dev/drbd999".to_owned(),
                device_path: "/dev/drbd999".to_owned(),
            }],
        )
        .expect_err("a mapping that matches no config disk is refused");
    assert_eq!(
        error.code,
        ApiErrorCode::InvalidState,
        "the divergent-snapshot refusal is typed: {error}"
    );
    // The refusals destroyed nothing: the VMM holds no VM, and the
    // real artifacts are untouched.
    assert_eq!(
        target.state(&vm).expect("the destination VM state"),
        VmState::Absent,
        "the refused restores left no half-restored VM"
    );

    evidence.invariant(
        "g5_no_resume_over_barrier",
        &assert_g5_complete(&rig, "mig-tv3").await,
    );
    evidence.invariant(
        "divergent_refusals_typed",
        "pass: the non-empty-VMM restore and the no-matching-disk restore are typed \
         refusals over the real api socket; nothing was half-restored",
    );
    evidence.outcome(
        "complete: the write-trace oracle crossed a real snapshot/restore pair — the \
         acknowledged prefix byte-intact at the destination, the boundary at the REAL \
         pause, and the divergent restores refused typed over the real api socket",
    );
    emit(
        &rig,
        evidence,
        &acked,
        boundary,
        "destination",
        Some(&summary),
    )
}

// ------------------------------------------------- scenario (d): the row

/// Scenario (d): the P6 grow notification over a real VMM —
/// GrowVolume's `vm.resize-disk` notification against a real
/// cloud-hypervisor at a recorded version, with the `[vmm]
/// minimum_version` gate probed from the real binary's `--version`
/// output, the fail-closed retry contract, and the retry pass.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tier_v_resize_notification() {
    let record = tier_v::drive(tier_v::scenario("resize-notification"), |env| async move {
        serialized(real_resize_notification(env)).await
    })
    .await;
    assert!(
        record.is_file(),
        "the tier-v/resize-notification record landed: {record:?}"
    );
}

/// The admin bearer token (fail-closed auth on every mutating route).
const ADMIN_TOKEN: &str = "tier-v-grow-admin-token";
/// The device-claim destructive-authorization token.
const CLAIM_TOKEN: &str = "tier-v-grow-claim-token";
/// Simulated pool capacity (1 TiB).
const POOL_BYTES: u64 = 1 << 40;
/// Simulated physical extent size (4 MiB), as in real LVM.
const EXTENT_BYTES: u64 = 4 << 20;

// ------------------------------------------- the scripted LVM world (P6)

/// The compact simulated LVM world this scenario needs (the
/// `grow_e2e.rs` precedent): LVs and VG free space in a shared map,
/// `lvcreate`/`lvextend` mutating it, `lvs`/`vgs` reporting it — so
/// the provider's verification steps (sizes read back from `lvs`)
/// exercise real round-trips instead of echoes.
/// `lvcreate`/`lvextend` round sizes up to whole extents, as real
/// LVM does. The storage below the seam is simulated (ADR-0010's
/// allowance); the VMM the notification addresses is real.
struct ScriptedLvm {
    /// LV full path (`vg/lv`) to size in bytes.
    lvs: BTreeMap<String, u64>,
    /// PV device paths known to `pvs`.
    pvs: Vec<String>,
    /// VG name to free bytes (before any LV allocations).
    vg_free: BTreeMap<String, u64>,
    /// VG name to total bytes.
    vg_size: BTreeMap<String, u64>,
    /// Physical extent size.
    extent_size: u64,
}

impl ScriptedLvm {
    fn new() -> Self {
        Self {
            lvs: BTreeMap::new(),
            pvs: Vec::new(),
            vg_free: BTreeMap::new(),
            vg_size: BTreeMap::new(),
            extent_size: EXTENT_BYTES,
        }
    }

    /// The scripted runner wired to this world (closure mode).
    fn runner(world: &Arc<Mutex<Self>>) -> Arc<FakeRunner> {
        let world = Arc::clone(world);
        Arc::new(FakeRunner::with_closure(move |program, args| {
            let mut world = world.lock().ok()?;
            script(&mut world, program, args)
        }))
    }
}

/// A JSON report wrapper: `{"report":[{"<key>":[rows]}]}`.
fn report(key: &str, rows: &[Value]) -> CommandOutput {
    CommandOutput::success(json!({ "report": [{ key: rows }] }).to_string())
}

/// The `lvs` report rows for the simulated world.
fn lvs_report(world: &ScriptedLvm) -> CommandOutput {
    let rows: Vec<Value> = world
        .lvs
        .iter()
        .map(|(path, size)| {
            let (vg, lv) = path.split_once('/').expect("path is vg/lv");
            json!({
                "vg_name": vg,
                "lv_name": lv,
                "lv_size": size.to_string(),
            })
        })
        .collect();
    report("lv", &rows)
}

/// The `vgs` report rows for the simulated world.
fn vgs_report(world: &ScriptedLvm) -> CommandOutput {
    let rows: Vec<Value> = world
        .vg_free
        .keys()
        .map(|vg| {
            let allocated: u64 = world
                .lvs
                .iter()
                .filter_map(|(path, size)| {
                    path.split_once('/')
                        .and_then(|(vg_name, _)| (vg_name == vg).then_some(*size))
                })
                .sum();
            let free = world
                .vg_free
                .get(vg)
                .copied()
                .unwrap_or_default()
                .saturating_sub(allocated);
            json!({
                "vg_name": vg,
                "vg_free": free.to_string(),
                "vg_size": world.vg_size.get(vg).copied().unwrap_or_default().to_string(),
                "vg_extent_size": world.extent_size.to_string(),
            })
        })
        .collect();
    report("vg", &rows)
}

/// The `lsblk` report (the discovery surface: one fixture disk).
fn lsblk_report() -> CommandOutput {
    CommandOutput::success(
        json!({
            "blockdevices": [
                {"name": "loop0", "type": "loop", "size": 104_857_600,
                 "serial": null, "wwn": null, "model": null},
                {"name": "sda", "type": "disk", "size": POOL_BYTES,
                 "serial": "FIXTURE-SERIAL-1", "wwn": "0x5000c500fixt0001",
                 "model": "Fixture Disk"},
            ]
        })
        .to_string(),
    )
}

/// The flag value after `flag` in `args`.
fn arg_after<'a>(args: &[&'a str], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|arg| *arg == flag)
        .and_then(|index| args.get(index + 1))
        .copied()
}

/// Round `size` up to whole extents.
fn round_up_to_extent(size: u64, extent: u64) -> u64 {
    if extent == 0 {
        return size;
    }
    size.div_ceil(extent) * extent
}

/// The scripted command surface.
fn script(world: &mut ScriptedLvm, program: &str, args: &[&str]) -> Option<CommandOutput> {
    match program {
        "lvs" => Some(lvs_report(world)),
        "vgs" => Some(vgs_report(world)),
        "pvs" => {
            let rows: Vec<Value> = world
                .pvs
                .iter()
                .map(|path| json!({ "pv_name": path }))
                .collect();
            Some(report("pv", &rows))
        }
        "lsblk" => Some(lsblk_report()),
        "lvcreate" => {
            // lvcreate --yes -L <size>B -n <lv> <vg>
            let size = arg_after(args, "-L")?;
            let size: u64 = size.trim_end_matches('B').parse().ok()?;
            let lv = arg_after(args, "-n")?;
            let vg = *args.last()?;
            let path = format!("{vg}/{lv}");
            if world.lvs.contains_key(&path) {
                return Some(CommandOutput::failure(format!(
                    "lvcreate: {path} already exists"
                )));
            }
            let effective = round_up_to_extent(size, world.extent_size);
            world.lvs.insert(path, effective);
            Some(CommandOutput::success(String::new()))
        }
        "lvextend" => {
            // lvextend --yes -L <size>B <vg>/<lv>
            let size = arg_after(args, "-L")?;
            let size: u64 = size.trim_end_matches('B').parse().ok()?;
            let path = *args.last()?;
            let effective = round_up_to_extent(size, world.extent_size);
            world.lvs.insert(path.to_owned(), effective);
            Some(CommandOutput::success(String::new()))
        }
        "pvcreate" => {
            world.pvs.push((*args.last()?).to_owned());
            Some(CommandOutput::success(String::new()))
        }
        "vgcreate" => {
            // vgcreate --yes <vg> <path>
            let vg = (*args.get(1)?).to_owned();
            world.vg_free.insert(vg.clone(), POOL_BYTES);
            world.vg_size.insert(vg, POOL_BYTES);
            Some(CommandOutput::success(String::new()))
        }
        _ => None,
    }
}

/// The grow scenario's durable world: the scripted LVM, the journal
/// directory (journal, LVM state, notification store).
struct GrowWorld {
    /// Kept alive for the scenario's lifetime.
    _dir: tempfile::TempDir,
    /// The simulated LVM.
    lvm: Arc<Mutex<ScriptedLvm>>,
    /// The journal directory.
    journal_dir: PathBuf,
}

impl GrowWorld {
    fn new() -> Arc<Self> {
        let dir = tempfile::tempdir().expect("tempdir");
        Arc::new(Self {
            journal_dir: dir.path().join("journal"),
            lvm: Arc::new(Mutex::new(ScriptedLvm::new())),
            _dir: dir,
        })
    }

    /// The provider over the durable state file and the scripted LVM.
    fn provider(&self) -> Arc<LvmProvider> {
        let runner: Arc<dyn CommandRunner> = ScriptedLvm::runner(&self.lvm);
        LvmProvider::new(
            runner,
            self.journal_dir.join("lvm-state.json"),
            PathBuf::from("/nonexistent-sysfs"),
            "vvtier5".to_owned(),
            CLAIM_TOKEN.to_owned(),
        )
        .map(Arc::new)
        .expect("provider construction")
    }

    /// The durable notification store's path.
    fn store_path(&self) -> PathBuf {
        self.journal_dir.join("notifications")
    }

    /// The volume's LV size in the world (read back through the
    /// scripted `lvs`, never the response; the scenario creates
    /// exactly one LV, and the provider's VG/LV names are
    /// hash-derived — the first LV IS the volume).
    fn backing_size(&self) -> u64 {
        let world = self.lvm.lock().expect("the lvm world");
        let (_, size) = world
            .lvs
            .iter()
            .next()
            .expect("exactly one LV exists after create");
        *size
    }
}

/// One grow-daemon incarnation: the real API router over the LVM
/// provider, the grow engine wired to the REAL VMM controller and
/// the probed version gate. The composition is `grow_e2e.rs`'s, with
/// the fake VMM replaced by the real adapter over the live process.
struct GrowDaemon {
    addr: SocketAddr,
    engine: Arc<GrowNotificationEngine>,
    world: Arc<GrowWorld>,
    serve: Option<JoinHandle<()>>,
}

impl GrowDaemon {
    /// Launch one incarnation: `vmm` is the REAL controller, `gate`
    /// the probed version gate (proven or refused — both legs are
    /// the scenario's body).
    async fn launch(
        world: &Arc<GrowWorld>,
        vmm: Arc<dyn VmmController>,
        gate: VmmVersionGate,
    ) -> Self {
        let provider = world.provider();
        let store =
            GrowNotificationStore::open(world.store_path()).expect("open the notification store");
        let facts: GrowAttachmentFacts = {
            let provider = Arc::clone(&provider);
            Arc::new(move || provider.grow_attachment_facts())
        };
        let clock: volvisor_provider::grow::Clock = {
            let ticks = Arc::new(AtomicU64::new(1_700_000_000));
            Arc::new(move || ticks.fetch_add(1, Ordering::SeqCst))
        };
        let engine = Arc::new(GrowNotificationEngine::new(
            Some(vmm),
            gate,
            facts,
            store,
            clock,
        ));
        let admin: Arc<dyn volvisor_provider::AdminSurface> = provider.clone();
        let state = AppState::new(
            Arc::clone(&provider) as Arc<dyn volvisor_provider::VolumeProvider>,
            Some(admin),
            Journal::open(&world.journal_dir).expect("journal opens"),
            Some(ADMIN_TOKEN.to_owned()),
        )
        .with_grow_notifier(Arc::clone(&engine) as Arc<dyn GrowNotifier>);
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the grow daemon");
        let addr = listener.local_addr().expect("the grow daemon's local addr");
        let app = router(Arc::new(state), 1 << 20);
        let serve = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("the grow daemon serves");
        });
        Self {
            addr,
            engine,
            world: Arc::clone(world),
            serve: Some(serve),
        }
    }

    /// Stop the incarnation (the aborted serve task drops the
    /// router state, releasing the journal's flock).
    async fn stop(&mut self) {
        if let Some(serve) = self.serve.take() {
            serve.abort();
            let _ = serve.await;
        }
    }
}

/// One admin-authenticated call against the grow daemon.
async fn grow_admin(
    method: &str,
    addr: SocketAddr,
    path: &str,
    body: Option<&str>,
) -> (u16, String) {
    let mut stream = tokio::net::TcpStream::connect(addr)
        .await
        .expect("connect the grow daemon");
    let body = body.unwrap_or("");
    let request = format!(
        "{method} {path} HTTP/1.1\r\nhost: localhost\r\ncontent-type: application/json\r\n\
         content-length: {}\r\nauthorization: Bearer {ADMIN_TOKEN}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write the grow request");
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .expect("read the grow response");
    let text = String::from_utf8_lossy(&response).into_owned();
    let status: u16 = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .expect("the grow status line");
    let body_start = text
        .find("\r\n\r\n")
        .map(|i| text[i + 4..].to_owned())
        .unwrap_or_default();
    (status, body_start)
}

/// The scenario's volume id.
fn volume_id() -> VolumeId {
    VolumeId::new("vol-grow").expect("valid volume id")
}

/// The claim → create → attach sequence (the attach names the REAL
/// VM id and its VMM disk id — the notification's target).
async fn claim_create_attach(daemon: &GrowDaemon, vm: &str, vmm_disk_id: &str) {
    let devices = grow_admin("GET", daemon.addr, "/v2/admin/devices", None).await;
    assert_eq!(devices.0, 200, "device list: {}", devices.1);
    let device_id = body_json(&devices.1)["devices"][0]["id"]
        .as_str()
        .expect("a discovered device id")
        .to_owned();
    let claim = format!(
        "{{\"api_version\":\"volvisor.volume.v2\",\"operation_id\":\"op-claim\",\
         \"authorization_token\":\"{CLAIM_TOKEN}\"}}"
    );
    let (status, body) = grow_admin(
        "POST",
        daemon.addr,
        &format!("/v2/admin/devices/{device_id}/claim"),
        Some(&claim),
    )
    .await;
    assert_eq!(status, 200, "claim: {body}");

    let create = format!(
        "{{\"api_version\":\"volvisor.volume.v2\",\"operation_id\":\"op-create\",\
         \"project_id\":\"tier-v\",\"volume_id\":\"vol-grow\",\"class\":\"native-local\",\
         \"size_bytes\":{GIB}}}"
    );
    let (status, body) = grow_admin("POST", daemon.addr, "/v2/volumes", Some(&create)).await;
    assert_eq!(status, 200, "create: {body}");

    let attach = format!(
        "{{\"api_version\":\"volvisor.volume.v2\",\"operation_id\":\"op-attach\",\
         \"vm_id\":\"{vm}\",\"host_id\":\"host-1\",\"attachment_id\":\"att-1\",\
         \"expected_volume_generation\":1,\"vmm_disk_id\":\"{vmm_disk_id}\"}}"
    );
    let (status, body) = grow_admin(
        "POST",
        daemon.addr,
        "/v2/volumes/vol-grow/attach",
        Some(&attach),
    )
    .await;
    assert_eq!(status, 200, "attach: {body}");
}

/// The grow act (the public route).
async fn grow(
    daemon: &GrowDaemon,
    operation_id: &str,
    new_size_bytes: u64,
    expected_generation: u64,
) -> (u16, String) {
    let body = format!(
        "{{\"api_version\":\"volvisor.volume.v2\",\"operation_id\":\"{operation_id}\",\
         \"new_size_bytes\":{new_size_bytes},\"expected_generation\":{expected_generation}}}"
    );
    grow_admin(
        "POST",
        daemon.addr,
        "/v2/volumes/vol-grow/grow",
        Some(&body),
    )
    .await
}

/// Grow the real disk file out-of-band (the operator's act the
/// notification follows).
fn grow_disk_file(path: &Path, bytes: u64) {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open the real disk file");
    file.set_len(bytes).expect("grow the real disk file");
}

async fn real_resize_notification(env: RealVmmEnv) -> PathBuf {
    let seq = next_id();
    let vm = format!("vm-tv4-{seq}");
    // The SINGLE shape: one real VMM, one real disk file under the
    // rig's directory (the LVM world below the seam is scripted;
    // the notification addresses the VMM's disk id).
    let real = Arc::new(
        RealVmmRig::launch_single(env.clone(), &vm, GIB)
            .await
            .expect("the claimed environment backs the real-VMM launch"),
    );
    real.boot_source_vm()
        .await
        .expect("the real guest reaches the busybox shell");
    assert_eq!(
        real.source_vmm().state(&vm).expect("the VM state"),
        VmState::Running,
        "the real guest is running before the grow"
    );
    let disk_id = real.disk_ids()[0].clone();
    let disk_path = real.disk_paths()[0].clone();

    let mut evidence = Evidence::new_tier_v("tier-v/resize-notification");
    evidence.components(env.components_value());

    // ---- leg 1: the PROVEN version gate (37.0.0 ≤ the real
    // binary's version) — the notification reaches the real VMM.
    let vmm: Arc<dyn VmmController> = Arc::new(real.source_vmm());
    let world = GrowWorld::new();
    let mut daemon = GrowDaemon::launch(
        &world,
        Arc::clone(&vmm),
        VmmVersionGate::probe(
            Some(&env.cloud_hypervisor),
            Some("37.0.0"),
            &RealRunner::default(),
        ),
    )
    .await;
    claim_create_attach(&daemon, &vm, &disk_id).await;

    // The operator grows the real backing out-of-band, then the
    // volume through the public route.
    grow_disk_file(&disk_path, 2 * GIB);
    let (status, body) = grow(&daemon, "op-grow-1", 2 * GIB, 2).await;
    assert_eq!(status, 200, "the grow itself succeeds: {body}");
    let value = body_json(&body);
    assert_eq!(value["effective_size_bytes"], json!(2 * GIB), "{body}");
    // The backing really grew (read back through the simulated lvs).
    assert_eq!(daemon.world.backing_size(), 2 * GIB);
    // The notification's composed status is honest — one of exactly
    // two answers, both of which this scenario records:
    let notified = value["guest_notification_status"] == json!("notified");
    let retry_required = value["guest_notification_status"] == json!("retry_required");
    assert!(
        notified || retry_required,
        "the notification's composed status is one of the two honest answers: {body}"
    );

    let finding = if retry_required {
        // The fail-closed contract: the typed refusal is carried,
        // the durable record says retry_required with the reason,
        // and the never-shrink rule holds on the backing.
        let record = daemon
            .engine
            .record(&volume_id())
            .expect("the record")
            .expect("the durable notification record exists");
        let reason = record.pending_reason().expect("pending with a reason");
        assert!(
            reason.contains("404") || reason.contains("refused"),
            "the pending reason carries the real VMM's typed answer: {reason}"
        );
        // The retry pass re-drives the notification against the real
        // VMM: the record stays retry_required (the route is still
        // absent at this binary's version) — the honest durable
        // state, observed rather than assumed.
        let report = daemon.engine.retry_pass().expect("the retry pass");
        assert!(
            !report.retry_required.is_empty(),
            "the retry pass honestly re-reports the missing route"
        );
        format!(
            "finding: the real binary answers vm.resize-disk with the typed refusal \
             carried in the durable record ({reason}); the fail-closed contract held — \
             the grow completed, the notification is retry_required, never a silent \
             un-notified success"
        )
    } else {
        let report = daemon.engine.retry_pass().expect("the retry pass");
        assert!(
            report.retry_required.is_empty(),
            "a notified record leaves nothing to retry"
        );
        "the notification landed (a 2xx from the real VMM's resize-disk \
         route at this binary's version)"
            .to_owned()
    };
    // The VM survived the notification attempt either way.
    assert_eq!(
        real.source_vmm().state(&vm).expect("the VM state"),
        VmState::Running,
        "the real guest survives the notification attempt"
    );
    daemon.stop().await;

    // ---- leg 2: the REFUSED version gate (99.0.0 > the real
    // binary's version) — the notification is refused BEFORE any
    // call to the VMM, recorded with the gate's reason.
    let world = daemon.world;
    let mut refused = GrowDaemon::launch(
        &world,
        vmm,
        VmmVersionGate::probe(
            Some(&env.cloud_hypervisor),
            Some("99.0.0"),
            &RealRunner::default(),
        ),
    )
    .await;
    grow_disk_file(&disk_path, 3 * GIB);
    let (status, body) = grow(&refused, "op-grow-2", 3 * GIB, 3).await;
    assert_eq!(status, 200, "the grow itself succeeds: {body}");
    let value = body_json(&body);
    assert_eq!(
        value["guest_notification_status"],
        json!("retry_required"),
        "the refused gate fails the notification closed: {body}"
    );
    let record = refused
        .engine
        .record(&volume_id())
        .expect("the record")
        .expect("the durable notification record exists");
    let reason = record.pending_reason().expect("pending with a reason");
    assert!(
        reason.contains("version") || reason.contains("gate"),
        "the refused gate's reason names the gate: {reason}"
    );
    assert_eq!(
        real.source_vmm().state(&vm).expect("the VM state"),
        VmState::Running,
        "the refused gate never reached the VMM"
    );
    refused.stop().await;

    evidence.invariant(
        "notification_contract_both_ways",
        &format!(
            "pass: the notification's composed status is a real observation ({finding}); \
             the refused version gate fails closed before any VMM call"
        ),
    );
    evidence.invariant(
        "version_gate_probed_from_the_real_binary",
        "pass: both gate legs probed the real binary's --version output through the \
         real runner (proven minimum 37.0.0, refused minimum 99.0.0)",
    );
    evidence.outcome(
        "complete: the P6 grow notification was driven against a real VMM — both \
         version-gate legs probed from the real binary, the fail-closed retry contract, \
         and the retry pass",
    );
    let logs = LogSources {
        a_journal: &world.journal_dir,
        b_journal: &world.journal_dir,
        witness: &world.journal_dir,
    };
    evidence.finish(&logs)
}
