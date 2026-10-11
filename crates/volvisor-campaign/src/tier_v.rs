//! Tier V — the real-VMM verification tier (ADR-0010, plan stage
//! P8-A).
//!
//! Tier S proves the implemented logic under the bounded injected
//! fault space; Tier R is the open hardware gate. **Tier V is the
//! tier between them**: the VMM process is REAL — a live
//! cloud-hypervisor driven through the real `ch-remote` adapter —
//! while the storage below the seam stays simulated (ADR-0010's
//! explicit allowance: the fake DRBD worlds, file-backed disks at
//! the DRBD device paths). What the tier verifies is the SEAM: the
//! adapter's pinned command surface, the pause/snapshot/destroy/
//! restore/resume state machine, and the migration protocol's VMM
//! acts, against a real VMM's real timing, real snapshot artifacts
//! and real refusals — never against their documented behavior
//! alone.
//!
//! # The env gate
//!
//! `VOLVISOR_TEST_VMM=1` claims real-VMM execution. Unset or any
//! other value is the default **skipped** gate: every scenario
//! emits its skip record (the matrix shows the gate, never a hole),
//! and the default suite asserts those records exist. The gate is
//! deliberately separate from Tier R's
//! (`VOLVISOR_CAMPAIGN_TIER=R`): one claims a real VMM, the other
//! claims real media — a host may carry either, both or neither.
//!
//! A claimed gate must be **backed** (ADR-0010's fail-loudly rule):
//! the detection resolves both binaries, `/dev/kvm`, and the boot
//! assets, and stamps each binary's `--version` output AND file
//! SHA256. Anything missing fails the scenario loudly before any
//! record is written — a claimed-but-unbacked tier is the one
//! fabrication this module must never commit.
//!
//! Overridable inputs (defaults in parentheses):
//!
//! | Variable | Meaning | Default |
//! |---|---|---|
//! | `VOLVISOR_TEST_VMM_CLOUD_HYPERVISOR` | the VMM binary | `cloud-hypervisor` on `PATH` |
//! | `VOLVISOR_TEST_VMM_CLOUD_HYPERVISOR_LEGACY` | an optional second, older VMM binary for the resize scenario's version-gate boundary leg (explicit only — never a `PATH` fallback, which would re-find the primary) | unset — the leg is honestly absent from the record |
//! | `VOLVISOR_TEST_VMM_CH_REMOTE` | the adapter binary | `ch-remote` on `PATH` |
//! | `VOLVISOR_TEST_VMM_KERNEL` | direct-boot kernel | `/boot/vmlinuz-$(uname -r)` |
//! | `VOLVISOR_TEST_VMM_INITRAMFS` | direct-boot initramfs | `/boot/initrd.img-$(uname -r)` |
//!
//! The boot strategy is **direct kernel boot** (chosen after
//! experiment, P8-A): the host kernel + initramfs with
//! `console=ttyS0` reaches a busybox `(initramfs)` shell in seconds
//! — a genuinely running guest (kernel booted, init ran, userspace
//! shell alive) without any disk image; the guest issues no disk
//! I/O, which is honest for this tier: the write workload is the
//! rig's writer model through the simulated device (ADR-0010's
//! allowance), and a real guest writer over the real device is
//! Tier R's body, recorded as such. The Noble-image + hypervisor-fw
//! fallback was not needed.
//!
//! # The claim vocabulary (CG5's per-tier scan)
//!
//! A Tier V record may claim exactly one thing: **verified against
//! a real VMM process at recorded versions**. It may never claim
//! production support, real-host durability, real DRBD or real
//! media — the storage below the seam is simulated and says so in
//! every record's `components`. A run record that does not name the
//! exact VMM it verified against (version + SHA256) fails the
//! summary's shape-coherence gate.
//!
//! # The CI posture
//!
//! The GitHub runners carry no KVM and no Cloud Hypervisor: Tier V
//! stays skipped-by-default there, and the skip records ARE the
//! gate (the same posture as Tier R). The release certification
//! runs green over skipped Tier V records by design.
//!
//! # What the first real run found (recorded, not papered over)
//!
//! The tier's purpose is exactly this: two doc-verified assumptions
//! did not survive contact with the installed pair
//! (cloud-hypervisor v43.0.0 + ch-remote v51.1):
//!
//! - A live VMM with no VM answers `info` with the body text `VM is
//!   not created`, which matched no not-found token — the
//!   empty-VMM shape (a restore target's steady state) surfaced as
//!   a typed `INTERNAL` instead of `Absent`. The token is now
//!   pinned in `volvisor-provider`'s `NOT_FOUND_TOKENS`.
//! - The P6-B notification route `PUT /api/v1/vm.resize-disk` does
//!   not exist at cloud-hypervisor v43.0.0 (the binary carries no
//!   such route; upstream added it by v51.0.0 — issue #7923 — and
//!   PR #7948 fixed its externally-grown host-block-device
//!   behavior in April 2026). The notification's fail-closed
//!   contract held exactly as designed: the typed refusal is
//!   carried, the grow reports `retry_required`, never a silent
//!   un-notified success. The route follow-up is recorded in the
//!   `resize-notification` scenario's evidence.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use volvisor_provider::vmm_http::put_json;
use volvisor_provider::{
    ChRemoteConfig, ChRemoteVmm, DiskMapping, FakeVmm, PauseProof, RealRunner, VmState,
    VmmController,
};
use volvisor_types::{ApiError, ApiErrorCode};

use crate::evidence::Evidence;
use crate::rig::VmmControllerFactory;

// ---------------------------------------------------------- the env gate

/// The Tier V env gate (ADR-0010): `VOLVISOR_TEST_VMM=1` claims
/// real-VMM execution. Unset or any other value is the default
/// skipped gate — the scenarios emit their skip records and the
/// suite stays green (the GitHub CI posture).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gate {
    /// The default: no real VMM is claimed; scenarios emit their
    /// skip records.
    Skipped,
    /// `VOLVISOR_TEST_VMM=1`: real-VMM execution is claimed — the
    /// environment must back it (fail loudly otherwise).
    Claimed,
}

/// The pure gate parse (unit-testable without env races).
#[must_use]
pub fn gate_from(value: Option<&str>) -> Gate {
    if value == Some("1") {
        Gate::Claimed
    } else {
        Gate::Skipped
    }
}

/// Read the env gate.
#[must_use]
pub fn env_gate() -> Gate {
    gate_from(std::env::var("VOLVISOR_TEST_VMM").ok().as_deref())
}

// ------------------------------------------------- the real environment

/// The detected real-VMM environment: the binaries, the boot
/// assets, and the stamps every Tier V record carries (ADR-0010: a
/// run that does not name the exact VMM it verified against is not
/// evidence).
#[derive(Clone, Debug)]
pub struct RealVmmEnv {
    /// The cloud-hypervisor binary (resolved, existing).
    pub cloud_hypervisor: PathBuf,
    /// The ch-remote binary (resolved, existing).
    pub ch_remote: PathBuf,
    /// The direct-boot kernel.
    pub kernel: PathBuf,
    /// The direct-boot initramfs.
    pub initramfs: PathBuf,
    /// `/dev/kvm`, observed present.
    pub kvm: PathBuf,
    /// The cloud-hypervisor `--version` first line (e.g.
    /// `cloud-hypervisor v43.0.0`).
    pub cloud_hypervisor_version: String,
    /// The ch-remote `--version` first line (e.g. `ch-remote
    /// v51.1`).
    pub ch_remote_version: String,
    /// The cloud-hypervisor binary's SHA256 (64 lowercase hex).
    pub cloud_hypervisor_sha256: String,
    /// The ch-remote binary's SHA256 (64 lowercase hex).
    pub ch_remote_sha256: String,
    /// An optional second, older cloud-hypervisor binary claimed for
    /// the resize scenario's version-gate boundary leg: the same
    /// minimum probed against a binary below it — refused
    /// fail-closed, recorded from the real binary's `--version`
    /// output. Absent unless
    /// `VOLVISOR_TEST_VMM_CLOUD_HYPERVISOR_LEGACY` claims it.
    pub legacy_cloud_hypervisor: Option<LegacyCloudHypervisor>,
}

/// The optional older cloud-hypervisor binary the resize scenario's
/// boundary leg probes (ADR-0010's stamp discipline applies to it
/// too: a probed binary is named with its version + SHA256, never an
/// anonymous `--version` output).
#[derive(Clone, Debug)]
pub struct LegacyCloudHypervisor {
    /// The binary (resolved, existing — the claim is explicit, never
    /// a `PATH` fallback that would re-find the primary).
    pub path: PathBuf,
    /// The `--version` first line (e.g. `cloud-hypervisor v43.0.0`).
    pub version: String,
    /// The binary's SHA256 (64 lowercase hex).
    pub sha256: String,
}

/// The running kernel's release (`/proc/sys/kernel/osrelease`) —
/// the default boot-asset derivation.
fn kernel_release() -> Result<String, String> {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|release| release.trim().to_owned())
        .map_err(|error| format!("cannot read /proc/sys/kernel/osrelease: {error}"))
}

/// Resolve a binary: the override (which must exist) or the name on
/// `PATH`.
fn resolve_binary(override_var: &str, name: &str) -> Result<PathBuf, String> {
    if let Ok(path) = std::env::var(override_var) {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
        return Err(format!(
            "{override_var}={} does not exist as a file (the claimed \
             environment must back the claim)",
            path.display()
        ));
    }
    let path = std::env::var_os("PATH").map_or_else(Vec::new, |unparsed| {
        std::env::split_paths(&unparsed).collect()
    });
    for dir in path {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(format!(
        "no {name} on PATH (set {override_var} to the absolute binary path)"
    ))
}

/// Resolve a boot asset: the override (which must exist) or the
/// default path (which must exist).
fn resolve_asset(override_var: &str, default: &Path) -> Result<PathBuf, String> {
    if let Ok(path) = std::env::var(override_var) {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
        return Err(format!(
            "{override_var}={} does not exist as a file (the claimed \
             environment must back the claim)",
            path.display()
        ));
    }
    if default.is_file() {
        return Ok(default.to_path_buf());
    }
    Err(format!(
        "the default boot asset {} does not exist (set {override_var} to \
         the absolute path)",
        default.display()
    ))
}

/// The first line of `<binary> --version` (trimmed).
fn version_first_line(binary: &Path) -> Result<String, String> {
    let output = Command::new(binary)
        .arg("--version")
        .output()
        .map_err(|error| format!("cannot run {} --version: {error}", binary.display()))?;
    if !output.status.success() {
        return Err(format!(
            "{} --version exited with {:?}",
            binary.display(),
            output.status.code()
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let first = stdout.lines().next().unwrap_or_default().trim();
    if first.is_empty() {
        return Err(format!(
            "{} --version printed an empty first line",
            binary.display()
        ));
    }
    Ok(first.to_owned())
}

/// A file's SHA256, streamed (64 lowercase hex). The buffer lives
/// on the heap: the binaries are tens of megabytes and the read
/// loop must not pin a large stack array (clippy's
/// `large_stack_arrays` discipline).
fn sha256_file(path: &Path) -> Result<String, String> {
    use std::io::Read;
    let mut file =
        std::fs::File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 65_536];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Detect the claimed environment. Every input must resolve; every
/// failure names exactly what is missing and how to override it —
/// the loud gate ADR-0010 demands.
///
/// # Errors
/// One `String` naming the first unbacked piece of the claim.
pub fn detect_real_vmm() -> Result<RealVmmEnv, String> {
    let cloud_hypervisor =
        resolve_binary("VOLVISOR_TEST_VMM_CLOUD_HYPERVISOR", "cloud-hypervisor")?;
    let ch_remote = resolve_binary("VOLVISOR_TEST_VMM_CH_REMOTE", "ch-remote")?;
    let release = kernel_release()?;
    let kernel = resolve_asset(
        "VOLVISOR_TEST_VMM_KERNEL",
        &PathBuf::from(format!("/boot/vmlinuz-{release}")),
    )?;
    let initramfs = resolve_asset(
        "VOLVISOR_TEST_VMM_INITRAMFS",
        &PathBuf::from(format!("/boot/initrd.img-{release}")),
    )?;
    let kvm = PathBuf::from("/dev/kvm");
    if !kvm.exists() {
        return Err(
            "/dev/kvm does not exist — a real-VMM claim requires KVM on this host \
                    (the GitHub CI runners never have it; that is the skipped gate's reason)"
                .to_owned(),
        );
    }
    // The optional legacy binary: an explicit claim only. Set but
    // missing is the loud failure (the same rule as every override);
    // unset is the honest absence — the boundary leg simply does not
    // run, and the record states what it observed.
    let legacy_cloud_hypervisor =
        if std::env::var_os("VOLVISOR_TEST_VMM_CLOUD_HYPERVISOR_LEGACY").is_some() {
            let path = resolve_binary(
                "VOLVISOR_TEST_VMM_CLOUD_HYPERVISOR_LEGACY",
                "cloud-hypervisor",
            )?;
            Some(LegacyCloudHypervisor {
                version: version_first_line(&path)?,
                sha256: sha256_file(&path)?,
                path,
            })
        } else {
            None
        };
    Ok(RealVmmEnv {
        cloud_hypervisor_version: version_first_line(&cloud_hypervisor)?,
        ch_remote_version: version_first_line(&ch_remote)?,
        cloud_hypervisor_sha256: sha256_file(&cloud_hypervisor)?,
        ch_remote_sha256: sha256_file(&ch_remote)?,
        legacy_cloud_hypervisor,
        cloud_hypervisor,
        ch_remote,
        kernel,
        initramfs,
        kvm,
    })
}

impl RealVmmEnv {
    /// The record's `components` override (§6): what REALLY ran.
    /// The VMM entry carries the version + SHA256 stamps; the
    /// storage entry states the simulated-below-the-seam truth.
    /// Never a claim the tier cannot back: no production support,
    /// no real-host durability, no real DRBD — the vocabulary
    /// CG5 scans for on Tier V records.
    #[must_use]
    pub fn components_value(&self) -> Value {
        let mut vmm = json!({
            "cloud_hypervisor": {
                "path": self.cloud_hypervisor.display().to_string(),
                "version": self.cloud_hypervisor_version,
                "sha256": self.cloud_hypervisor_sha256,
            },
            "ch_remote": {
                "path": self.ch_remote.display().to_string(),
                "version": self.ch_remote_version,
                "sha256": self.ch_remote_sha256,
            },
            "guest": "direct kernel boot (host kernel + initramfs to a busybox \
                      shell on ttyS0); the guest issues no disk I/O — the write \
                      workload is the rig's model through the simulated device",
            "storage_below_the_seam": "simulated (the campaign's fake DRBD worlds; \
                                       file-backed disks at the DRBD device paths)",
        });
        if let Some(legacy) = &self.legacy_cloud_hypervisor {
            vmm["legacy_cloud_hypervisor"] = json!({
                "path": legacy.path.display().to_string(),
                "version": legacy.version,
                "sha256": legacy.sha256,
                "role": "the resize scenario's version-gate boundary leg: its --version \
                         probed through the real runner, never a launched VMM",
            });
        }
        json!({
            "volvisord": "in-process daemons over the real router (the campaign rig)",
            "vmm": vmm,
            "drbd-tooling": "fake",
            "witness": "in-process",
        })
    }
}

// --------------------------------------------------------- the scenarios

/// One Tier V scenario's scaffold (the Tier R pattern): what it
/// covers, its skip reason, and the body it runs when the gate is
/// claimed and backed. Unlike Tier R, every body here IS
/// implemented — the skip record is the default environment's gate
/// statement, never a missing implementation.
pub struct Scenario {
    /// The scenario's short name (the record is `tier-v/<name>`).
    pub name: &'static str,
    /// §9's family label for the row.
    pub family: &'static str,
    /// The nearline §10 / SPEC-0002 class the scenario covers.
    pub class: &'static str,
    /// The canonical skip reason when the env gate is not claimed.
    pub skip_reason: &'static str,
    /// The documented body (§0's scenario portability).
    pub body: &'static str,
}

/// The uniform skip reason: the env gate (every Tier V scenario's
/// only reason to skip — the bodies are implemented).
const GATE_SKIP_REASON: &str = "VOLVISOR_TEST_VMM is not set — no real-VMM execution is \
                                claimed in this environment (the GitHub CI runners carry no \
                                KVM and no Cloud Hypervisor; this record is the tier's gate, \
                                never a hole)";

/// The bounded Tier V scenario set (ADR-0010's four).
pub const SCENARIOS: &[Scenario] = &[
    Scenario {
        name: "migration-happy-path",
        family: "the migration happy path over a real VMM",
        class: "the full mobility protocol against a real Cloud Hypervisor",
        skip_reason: GATE_SKIP_REASON,
        body: "boot a real guest (direct kernel boot to a busybox shell) in a real \
               cloud-hypervisor process with the volume's device file as its disk; drive \
               one migration through the public routes; verify the REAL pause (api-socket \
               Paused), the real snapshot artifacts, the source VM's real destruction, the \
               real restore into a pre-started empty destination VMM, and the real resume \
               (api-socket Running); verify the protocol invariants (G5 barrier, source \
               demoted, destination promoted) and record the VMM binaries' versions and \
               SHA256 stamps",
    },
    Scenario {
        name: "kill-window-mid-restore",
        family: "the pause/snapshot kill windows over a real VMM",
        class: "a daemon kill inside the cut's restore act against a real VMM; recovery \
                through the retry engine",
        skip_reason: GATE_SKIP_REASON,
        body: "arm the destination's peer_restore_vm journal kill; drive the migration \
               until the kill lands with the cut durable and the real pause/snapshot \
               complete; observe the VM states via the api sockets at each leg; restart \
               the destination then the source (the operator's clustering) and verify the \
               startup pass re-drives the real destroy/restore/resume to COMPLETE, with \
               the destination VM Running by api-socket observation and the re-drive \
               safety rules resolving the real VMM's states",
    },
    Scenario {
        name: "snapshot-restore-divergence",
        family: "snapshot/restore divergence detection over a real VMM",
        class: "the write-trace oracle across a real snapshot/restore pair, and the typed \
                refusals of a divergent snapshot",
        skip_reason: GATE_SKIP_REASON,
        body: "run the live writer through the simulated device while a real migration \
               cuts (real pause, snapshot, restore, resume); verify the oracle's verdict \
               — every acknowledged block present at the verified side, corruption zero, \
               the tail quantified, the boundary from the data path at the REAL pause; \
               then attempt the adapter's restore against the real destination VMM with \
               a mismatched disk mapping (the typed refusal: a snapshot that does not \
               match the migration's recorded disks is never accepted) and against the \
               non-empty destination VMM (the typed refusal)",
    },
    Scenario {
        name: "resize-notification",
        family: "the P6 grow notification over a real VMM",
        class: "GrowVolume's vm.resize-disk notification against a real VMM at a recorded \
                version (the [vmm] minimum_version gate exercised against the real binary)",
        skip_reason: GATE_SKIP_REASON,
        body: "boot a real guest with a real disk file; attach the volume naming the \
               VMM's disk id; grow the backing file out-of-band; GrowVolume through the \
               public route with the version gate pinned at the route's boundary \
               (51.0.0 — below every live-verified route-carrying binary, above every \
               live-verified route-absent one) and probed from the real binary's \
               --version output; observe the notification's typed outcome against the \
               real VMM — a 2xx completion, or the typed refusal with the HTTP status \
               carried and the durable retry_required record (the fail-closed contract) \
               — and the retry pass's re-drive; the always-refused control (99.0.0); \
               and, when an older binary is claimed via \
               VOLVISOR_TEST_VMM_CLOUD_HYPERVISOR_LEGACY, the same boundary minimum \
               probed against it — unproven, the notification refused before any VMM \
               call",
    },
];

/// The scenario's record name (`tier-v/<name>`).
#[must_use]
pub fn record_name(scenario: &Scenario) -> String {
    format!("tier-v/{}", scenario.name)
}

/// Look up a scenario by name (the tests' entry).
#[must_use]
pub fn scenario(name: &str) -> &'static Scenario {
    SCENARIOS
        .iter()
        .find(|candidate| candidate.name == name)
        .unwrap_or_else(|| panic!("no Tier V scenario named {name}"))
}

// ------------------------------------------------------------- the drive

/// The claimed arm's loud gate: a claimed environment that does not
/// back the claim is the one fabrication this module must never
/// commit (ADR-0010's fail-loudly rule), so the detection's own
/// detail aborts the scenario before any record is written.
///
/// # Panics
///
/// When `env` is `Err` (the detail names the unbacked piece).
fn claimed_env_or_panic(env: Result<RealVmmEnv, String>, scenario_name: &str) -> RealVmmEnv {
    env.unwrap_or_else(|detail| {
        panic!(
            "VOLVISOR_TEST_VMM=1 claims real-VMM execution but this environment \
             does not back the claim (scenario {scenario_name}): {detail}"
        )
    })
}

/// Drive one Tier V scenario: the skipped gate emits the scenario's
/// skip record; the claimed gate verifies the environment backs the
/// claim (loudly, before any record) and runs the body — the body
/// emits its own run record with the real components. Returns the
/// record's path either way (the skip record's or the body's), so
/// the caller asserts the never-silent-absence rule on every run.
///
/// # Panics
///
/// When the gate is claimed but the environment does not back it —
/// the one fabrication this module must never commit (ADR-0010's
/// fail-loudly rule). The body's own assertion failures surface as
/// the test's red, which is the honest outcome for a real run.
pub async fn drive<F, Fut>(scenario: &Scenario, body: F) -> PathBuf
where
    F: FnOnce(RealVmmEnv) -> Fut,
    Fut: Future<Output = PathBuf>,
{
    match env_gate() {
        Gate::Skipped => {
            let evidence = Evidence::new_tier_v(&record_name(scenario));
            evidence.finish_tier_v_skip(scenario.skip_reason, scenario.body)
        }
        Gate::Claimed => {
            let env = claimed_env_or_panic(detect_real_vmm(), &record_name(scenario));
            body(env).await
        }
    }
}

// ---------------------------------------------------- the mirrored VMM

/// A [`VmmController`] that forwards every act to a REAL
/// `ChRemoteVmm` and mirrors the observed state transitions into
/// the rig's fake view — the composition that makes the campaign
/// rig work over a real VMM.
///
/// Why the mirror exists (and why it is honest): the rig's storage
/// layer below the seam is simulated (ADR-0010's allowance), but
/// its device model — which minors are held open, gating the fake
/// DRBD's demote/promote — reads the fake view's VM table, and the
/// write-trace oracle's writer polls the view's VM state (its I/O
/// must end at the REAL pause). Without the mirror, the fake
/// world's model would diverge from the real VMM's acts (a destroy
/// that never releases the device blocks the demote; a writer that
/// never stops fakes the boundary). With it, every real act lands
/// in both places, atomically: the mirror runs inside the same
/// synchronous controller call the daemon's `spawn_blocking` task
/// executes, so a daemon kill can never split a real act from its
/// mirror.
///
/// The honest record statement (every Tier V record's
/// `components`): the VMM seam is real (every act the daemon drives
/// reaches a live cloud-hypervisor through the real ch-remote
/// adapter); the storage below the seam is simulated, and its
/// device model mirrors the real VMM's OBSERVED state transitions.
pub struct MirroredVmm {
    real: ChRemoteVmm,
    view: Arc<FakeVmm>,
}

impl MirroredVmm {
    /// Wrap `real` with `view` as the mirrored observation model.
    #[must_use]
    pub fn new(real: ChRemoteVmm, view: Arc<FakeVmm>) -> Self {
        Self { real, view }
    }

    /// The typed error for a mirror that refused to follow a
    /// successful real act: a rig bug (the view diverged from the
    /// real VMM), named as one — never a production shape.
    fn mirror_bug(act: &str, vm_id: &str, error: &ApiError) -> ApiError {
        ApiError::new(
            ApiErrorCode::Internal,
            format!(
                "mirrored VMM: the real {act} of VM {vm_id} succeeded but the rig's view \
                 refused to mirror it ({error}) — the view diverged from the real VMM, \
                 which is a rig bug, never a production shape"
            ),
        )
    }
}

impl VmmController for MirroredVmm {
    fn pause(&self, vm_id: &str) -> Result<PauseProof, ApiError> {
        // The real act first (its proof is the result the
        // coordinator consumes); the mirror follows only on
        // success, so a real refusal is never masked.
        let proof = self.real.pause(vm_id)?;
        self.view
            .pause(vm_id)
            .map_err(|error| Self::mirror_bug("pause", vm_id, &error))?;
        Ok(proof)
    }

    fn snapshot(&self, vm_id: &str, dir: &Path) -> Result<(), ApiError> {
        // REAL artifacts only — deliberately NOT mirrored: the
        // fake view's snapshot would overwrite the real
        // config.json/memory-ranges/state.json that the real
        // restore (and the view's own restore mirror, which
        // re-reads them) depend on. The view carries no snapshot
        // state of its own; there is nothing to mirror.
        self.real.snapshot(vm_id, dir)
    }

    fn destroy(&self, vm_id: &str) -> Result<(), ApiError> {
        self.real.destroy(vm_id)?;
        // Both destroys are re-drive-safe (an absent VM is a
        // no-op), so the mirror needs no tolerance clause.
        self.view
            .destroy(vm_id)
            .map_err(|error| Self::mirror_bug("destroy", vm_id, &error))
    }

    fn restore(&self, vm_id: &str, dir: &Path, disks: &[DiskMapping]) -> Result<(), ApiError> {
        self.real.restore(vm_id, dir, disks)?;
        // The view's restore re-reads the REAL artifacts in `dir`
        // (the real restore has already rewritten config.json per
        // the same mapping) and lands its own VM paused holding
        // the mapped devices — the device model below the seam
        // follows the real restore.
        self.view
            .restore(vm_id, dir, disks)
            .map_err(|error| Self::mirror_bug("restore", vm_id, &error))
    }

    fn resume(&self, vm_id: &str) -> Result<(), ApiError> {
        self.real.resume(vm_id)?;
        self.view
            .resume(vm_id)
            .map_err(|error| Self::mirror_bug("resume", vm_id, &error))
    }

    fn state(&self, vm_id: &str) -> Result<VmState, ApiError> {
        // The honest observation, unm mirrored: what the daemon
        // (and the scenario's invariants) read is what the real
        // VMM reports over its api socket.
        self.real.state(vm_id)
    }

    fn resize_disk(&self, vm_id: &str, disk_id: &str, new_size_bytes: u64) -> Result<(), ApiError> {
        // No mirror: the view's resize call log is a Tier S
        // assertion surface, and the resize scenarios assert the
        // REAL VMM's own typed answer.
        self.real.resize_disk(vm_id, disk_id, new_size_bytes)
    }
}

// ------------------------------------------------------ the real VMM rig

/// The guest memory the Tier V VMs run with (512 MiB: a real
/// busybox guest needs far less; every snapshot writes the full
/// size as `memory-ranges`, so this bounds the per-migration I/O).
const MEMORY_BYTES: u64 = 512 * 1024 * 1024;

/// The serial-console marker proving the guest is genuinely
/// running: the busybox `(initramfs)` shell prompt (kernel booted,
/// init ran, userspace alive).
const BOOT_MARKER: &str = "(initramfs)";

/// The bounded-wait budget for VMM startup and guest boot (the
/// campaign's bounded-polling discipline; never a bare sleep).
const BOOT_BUDGET: Duration = Duration::from_secs(45);

/// One real-VMM fixture: one or two cloud-hypervisor processes (the
/// pair shape boots the guest on the source and starts the
/// destination EMPTY — the restore target's steady state; the
/// single shape serves the resize scenarios), their api-socket
/// directories, and the real sparse disk files standing in for the
/// volume media. In the PAIR shape the disks live at the DRBD
/// device paths (`/dev/drbd{minor}`, one per volume — the
/// coordinator's declared paths; the migration's symmetric-minor
/// convention means one file serves both legs, which every record
/// states honestly). In the SINGLE shape the disk is a plain file
/// under the rig's directory (the resize scenarios' VM disk — the
/// LVM world below is scripted, and the notification addresses the
/// VMM's disk id, never a device path).
///
/// The pair rig owns `/dev/drbd{minor}`: it creates the files
/// (refusing to shadow anything that already exists) and removes
/// them on drop, together with the VMM processes. Scenarios hold
/// the `RealVmmRig` for as long as the campaign rig drives it —
/// declare it BEFORE the campaign rig so it drops after it.
pub struct RealVmmRig {
    env: RealVmmEnv,
    vm_id: String,
    source_dir: PathBuf,
    target_dir: Option<PathBuf>,
    source: Child,
    target: Option<Child>,
    disk_paths: Vec<PathBuf>,
    serial: PathBuf,
}

impl RealVmmRig {
    /// Launch the PAIR shape (the migration scenarios): two VMM
    /// processes over real sparse disk files at the given DRBD
    /// minors (`disk_bytes` each — match the fixture's seeded volume
    /// size, `GIB` in the campaign rig).
    ///
    /// # Errors
    /// A `String` naming the unbacked piece (an occupied device
    /// path, a spawn failure, a socket that never appeared).
    pub async fn launch_pair(
        env: RealVmmEnv,
        vm_id: &str,
        minors: &[u32],
        disk_bytes: u64,
    ) -> Result<Self, String> {
        assert!(
            !minors.is_empty(),
            "a pair real-VMM rig needs at least one volume minor"
        );
        // The real media: sparse files at the DRBD device paths the
        // coordinator's disk mappings declare. /dev is devtmpfs;
        // root can create them; a sparse file costs nothing until
        // written. The rig refuses to shadow an existing path — a
        // leftover from a crashed run is named, not reused.
        let mut disk_paths = Vec::new();
        for minor in minors {
            let path = PathBuf::from(format!("/dev/drbd{minor}"));
            if path.exists() {
                return Err(format!(
                    "{} already exists — the Tier V rig owns the /dev/drbd{{minor}} \
                     paths it seeds and refuses to shadow a file (a leftover from a \
                     crashed run: remove it, then re-run)",
                    path.display()
                ));
            }
            let file = std::fs::File::options()
                .write(true)
                .create_new(true)
                .open(&path)
                .map_err(|error| format!("create {}: {error}", path.display()))?;
            file.set_len(disk_bytes)
                .map_err(|error| format!("size {}: {error}", path.display()))?;
            drop(file);
            disk_paths.push(path);
        }
        let source_dir = volvisor_drbd_testkit::leak_tempdir();
        let target_dir = volvisor_drbd_testkit::leak_tempdir();
        let source = spawn_vmm(&env, &socket_path(&source_dir, vm_id), &source_dir)?;
        let target = spawn_vmm(&env, &socket_path(&target_dir, vm_id), &target_dir)?;
        await_socket(&socket_path(&source_dir, vm_id), "the source VMM").await?;
        await_socket(&socket_path(&target_dir, vm_id), "the destination VMM").await?;
        Ok(Self {
            serial: source_dir.join("serial.log"),
            env,
            vm_id: vm_id.to_owned(),
            source_dir,
            target_dir: Some(target_dir),
            source,
            target: Some(target),
            disk_paths,
        })
    }

    /// Launch the SINGLE shape (the resize scenarios): one VMM
    /// process, one plain sparse disk file under the rig's own
    /// directory (no `/dev` writes — the LVM world below the seam is
    /// scripted and the notification addresses the disk id).
    ///
    /// # Errors
    /// A `String` naming the unbacked piece.
    pub async fn launch_single(
        env: RealVmmEnv,
        vm_id: &str,
        disk_bytes: u64,
    ) -> Result<Self, String> {
        let source_dir = volvisor_drbd_testkit::leak_tempdir();
        let disk = source_dir.join("disk.img");
        let file = std::fs::File::options()
            .write(true)
            .create_new(true)
            .open(&disk)
            .map_err(|error| format!("create {}: {error}", disk.display()))?;
        file.set_len(disk_bytes)
            .map_err(|error| format!("size {}: {error}", disk.display()))?;
        drop(file);
        let source = spawn_vmm(&env, &socket_path(&source_dir, vm_id), &source_dir)?;
        await_socket(&socket_path(&source_dir, vm_id), "the VMM").await?;
        Ok(Self {
            serial: source_dir.join("serial.log"),
            env,
            vm_id: vm_id.to_owned(),
            source_dir,
            target_dir: None,
            source,
            target: None,
            disk_paths: vec![disk],
        })
    }

    /// The [`VmmControllerFactory`] for
    /// [`crate::rig::campaign_rig_volumes_with_vmm`]: each daemon's
    /// controller is its real `ChRemoteVmm` wrapped around the
    /// rig's wired fake view.
    #[must_use]
    pub fn factory(self: &Arc<Self>) -> VmmControllerFactory {
        let rig = Arc::clone(self);
        Arc::new(move |view_a, view_b| {
            let a: Arc<dyn VmmController> = Arc::new(MirroredVmm::new(rig.source_vmm(), view_a));
            let b: Arc<dyn VmmController> = Arc::new(MirroredVmm::new(rig.target_vmm(), view_b));
            (a, b)
        })
    }

    /// A real ch-remote adapter over the SOURCE VMM's socket
    /// directory (observation and consumer-side acts).
    #[must_use]
    pub fn source_vmm(&self) -> ChRemoteVmm {
        ChRemoteVmm::new(
            ChRemoteConfig {
                ch_remote_bin: self.env.ch_remote.clone(),
                api_socket_dir: self.source_dir.clone(),
            },
            Arc::new(RealRunner::default()),
        )
    }

    /// A real ch-remote adapter over the DESTINATION VMM's socket
    /// directory.
    ///
    /// # Panics
    /// In the single-VMM shape (no destination was launched).
    #[must_use]
    pub fn target_vmm(&self) -> ChRemoteVmm {
        let dir = self
            .target_dir
            .as_ref()
            .expect("the single-VMM shape has no destination VMM — use source_vmm()");
        ChRemoteVmm::new(
            ChRemoteConfig {
                ch_remote_bin: self.env.ch_remote.clone(),
                api_socket_dir: dir.clone(),
            },
            Arc::new(RealRunner::default()),
        )
    }

    /// The real disk files the booted guest's disks are backed by
    /// (the `/dev/drbd{minor}` paths in the pair shape, the rig's
    /// own file in the single shape) — the out-of-band-grow surface
    /// for the resize scenarios.
    #[must_use]
    pub fn disk_paths(&self) -> &[PathBuf] {
        &self.disk_paths
    }

    /// The VMM disk ids the booted guest's disks carry (the
    /// `vmm_disk_id` an attach names; one per volume, in minor
    /// order).
    #[must_use]
    pub fn disk_ids(&self) -> Vec<String> {
        (0..self.disk_paths.len())
            .map(|index| format!("{}-disk-{index}", self.vm_id))
            .collect()
    }

    /// The serial console log (the boot-wait's evidence).
    #[must_use]
    pub fn serial_log(&self) -> &Path {
        &self.serial
    }

    /// The detected environment the rig runs over.
    #[must_use]
    pub fn env(&self) -> &RealVmmEnv {
        &self.env
    }

    /// Boot the source's real guest: `PUT /api/v1/vm.create` with
    /// the direct-kernel payload (host kernel + initramfs,
    /// `console=ttyS0`, the volume device files as raw disks with
    /// stable ids, serial to a file), then `PUT /api/v1/vm.boot`,
    /// then the bounded boot wait on the serial log for the
    /// busybox shell prompt — the campaign's bounded-polling
    /// discipline, never a bare sleep. This is the CONSUMER's act
    /// (the rig is the consumer: volvisor never launches VMMs).
    ///
    /// # Errors
    /// A `String` naming the failed step with the VMM's answer.
    pub async fn boot_source_vm(&self) -> Result<(), String> {
        let disks: Vec<Value> = self
            .disk_paths
            .iter()
            .enumerate()
            .map(|(index, path)| {
                json!({
                    "path": path.display().to_string(),
                    "id": format!("{}-disk-{index}", self.vm_id),
                    "image_type": "Raw",
                })
            })
            .collect();
        let payload = json!({
            "cpus": {"boot_vcpus": 1, "max_vcpus": 1},
            "memory": {"size": MEMORY_BYTES},
            "payload": {
                "kernel": self.env.kernel.display().to_string(),
                "initramfs": self.env.initramfs.display().to_string(),
                "cmdline": "console=ttyS0",
            },
            "disks": disks,
            "serial": {"mode": "File", "file": self.serial.display().to_string()},
        });
        let create = put_json(
            &socket_path(&self.source_dir, &self.vm_id),
            "/api/v1/vm.create",
            &payload.to_string(),
            Duration::from_secs(30),
        )
        .map_err(|error| format!("vm.create over the real VMM: {error}"))?;
        if !(200..300).contains(&create.status) {
            return Err(format!(
                "vm.create answered {} {}",
                create.status, create.reason
            ));
        }
        // vm.boot takes no body: content-length 0 (the same
        // byte-pinned PUT path the adapter uses).
        let boot = put_json(
            &socket_path(&self.source_dir, &self.vm_id),
            "/api/v1/vm.boot",
            "",
            Duration::from_secs(30),
        )
        .map_err(|error| format!("vm.boot over the real VMM: {error}"))?;
        if !(200..300).contains(&boot.status) {
            return Err(format!("vm.boot answered {} {}", boot.status, boot.reason));
        }
        // The bounded boot wait: poll the serial log for the
        // busybox prompt.
        let deadline = Instant::now() + BOOT_BUDGET;
        while Instant::now() < deadline {
            if let Ok(log) = std::fs::read_to_string(&self.serial) {
                if log.contains(BOOT_MARKER) {
                    return Ok(());
                }
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        Err(format!(
            "the guest did not reach the {BOOT_MARKER} shell on {} within {}s (the \
             serial log is the evidence)",
            self.serial.display(),
            BOOT_BUDGET.as_secs()
        ))
    }
}

impl Drop for RealVmmRig {
    fn drop(&mut self) {
        // The processes die first (SIGKILL — a scenario's teardown
        // never waits on a guest shutdown), then the disk files the
        // rig owns leave (the /dev/drbd{minor} paths in the pair
        // shape; the rig's own file in the single shape). Best-effort
        // by design: the scenarios' assertions have already run; a
        // teardown failure would only mask their result.
        let _ = self.source.kill();
        if let Some(mut target) = self.target.take() {
            let _ = target.kill();
            let _ = target.wait();
        }
        let _ = self.source.wait();
        for path in &self.disk_paths {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// The per-VM api-socket path (the adapter's convention:
/// `{dir}/{vm_id}.sock`).
fn socket_path(dir: &Path, vm_id: &str) -> PathBuf {
    dir.join(format!("{vm_id}.sock"))
}

/// Spawn one EMPTY cloud-hypervisor process over `socket` (the
/// v43+ shape: `--api-socket` alone; the VM arrives via the REST
/// API or a restore — the restore target's steady state).
fn spawn_vmm(env: &RealVmmEnv, socket: &Path, log_dir: &Path) -> Result<Child, String> {
    let log = log_dir.join("cloud-hypervisor.log");
    let file = std::fs::File::create(&log)
        .map_err(|error| format!("create {}: {error}", log.display()))?;
    Command::new(&env.cloud_hypervisor)
        .arg("--api-socket")
        .arg(socket)
        .stdout(Stdio::from(file.try_clone().map_err(|error| {
            format!("clone {}'s handle: {error}", log.display())
        })?))
        .stderr(Stdio::from(file))
        .spawn()
        .map_err(|error| {
            format!(
                "spawn {} --api-socket {}: {error}",
                env.cloud_hypervisor.display(),
                socket.display()
            )
        })
}

/// The bounded wait for a VMM's api socket to appear (its listener
/// binds within ~100ms; the budget is generous for a loaded host).
async fn await_socket(socket: &Path, what: &str) -> Result<(), String> {
    let deadline = Instant::now() + BOOT_BUDGET;
    while Instant::now() < deadline {
        if socket.exists() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(format!(
        "{what} did not create the api socket {} within {}s",
        socket.display(),
        BOOT_BUDGET.as_secs()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A staging directory for the substrate's unit-test records —
    /// NEVER the live run directory (the Tier R precedent,
    /// round-1 review MAJOR-1): a unit test must not fabricate
    /// evidence into the tree the summary certifies.
    fn staging_dir() -> PathBuf {
        volvisor_drbd_testkit::leak_tempdir()
    }

    /// The gate parses exactly `VOLVISOR_TEST_VMM=1` — anything
    /// else (unset, empty, `0`, `true`) is the default skipped
    /// gate, so a stray variable can never silently claim a real
    /// VMM.
    #[test]
    fn the_gate_parses_only_the_exact_claim() {
        assert_eq!(gate_from(None), Gate::Skipped);
        assert_eq!(gate_from(Some("")), Gate::Skipped);
        assert_eq!(gate_from(Some("0")), Gate::Skipped);
        assert_eq!(gate_from(Some("true")), Gate::Skipped);
        assert_eq!(gate_from(Some("yes")), Gate::Skipped);
        assert_eq!(gate_from(Some("1")), Gate::Claimed);
    }

    /// A claimed gate without a backing environment fails loudly
    /// with the detection's own detail (never a silent pass).
    #[test]
    #[should_panic(expected = "does not back the claim")]
    fn a_claimed_gate_without_an_environment_fails_loudly() {
        claimed_env_or_panic(
            Err("no cloud-hypervisor on PATH (set \
                 VOLVISOR_TEST_VMM_CLOUD_HYPERVISOR to the absolute binary path)"
                .to_owned()),
            "tier-v/migration-happy-path",
        );
    }

    /// The skipped gate emits the §6 gate-statement shape: tier V,
    /// outcome skipped, `components` null (a skip observes nothing
    /// about the environment — the CG5 coherence clause's read-side
    /// twin), the canonical reason naming the env gate, and the
    /// documented body — persisted to a STAGING directory like every
    /// unit test of the substrate.
    #[test]
    fn the_skipped_gate_emits_the_gate_record_shape() {
        let scenario = scenario("migration-happy-path");
        let staging = staging_dir();
        let evidence = Evidence::new_tier_v(&record_name(scenario));
        let path = evidence.finish_tier_v_skip_at(&staging, scenario.skip_reason, scenario.body);
        let body = std::fs::read_to_string(&path).expect("the skip record landed in staging");
        let record: serde_json::Value =
            serde_json::from_str(&body).expect("the skip record is JSON");
        assert_eq!(record["scenario"], "tier-v/migration-happy-path");
        assert_eq!(record["tier"], "V");
        assert_eq!(record["outcome"], "skipped");
        assert!(
            record["components"].is_null(),
            "a skip observes nothing about the environment"
        );
        assert!(
            record["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("VOLVISOR_TEST_VMM")),
            "the skip reason names the env gate: {}",
            record["reason"]
        );
        assert_eq!(record["would_run"], scenario.body);
        assert!(
            !path.starts_with(crate::evidence::run_dir()),
            "the staged record never touched the live run directory"
        );
    }

    /// The scenario table carries ADR-0010's four bounded scenarios
    /// with unique names under the record-name convention the
    /// summary's CG1 presence loop expects.
    #[test]
    fn the_scenario_table_carries_the_four_bounded_scenarios() {
        let mut names: Vec<&str> = SCENARIOS.iter().map(|scenario| scenario.name).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "scenario names are unique");
        assert_eq!(total, 4, "ADR-0010's four bounded scenarios");
        for scenario in SCENARIOS {
            assert_eq!(
                record_name(scenario),
                format!("tier-v/{}", scenario.name),
                "the record-name convention holds"
            );
            assert!(
                !scenario.body.is_empty() && !scenario.skip_reason.is_empty(),
                "{} documents its body and its gate reason",
                scenario.name
            );
        }
    }
}
