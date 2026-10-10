//! The committed Tier V evidence bundle's shape (PR #27 review round
//! 1, MINOR-2): what any fresh checkout can check about
//! `docs/evidence/tier-v/` without running a single scenario.
//!
//! The honest boundary this test IS: the full certification
//! (`campaign-summary --all --check`) legitimately needs the whole
//! run TREE — CG1 asserts the Tier S/R matrix too, which lives only
//! in the gitignored `target/campaign-evidence/` — so a committed
//! Tier V directory alone can never (and must never) pass it. What
//! a committed directory CAN prove standalone is its own shape, and
//! that is pinned here: every record parses (with the campaign's
//! own read-side type, `serde_json::Value` — the same type the
//! summary parses run records into), carries the tier's record
//! shape (tier, the outcome vocabulary, the version + SHA256
//! stamps), names a scenario from the tier's table, and the
//! bundle's README agrees with its records wherever the agreement
//! is mechanical (the per-scenario table's outcome word and
//! duration, and the row count). A drift in any of these fails the
//! default suite from a fresh checkout — the committed bundle can
//! no longer rot silently.

// Test-kit code: invariant assertions may use expect/unwrap, and
// the discovery helpers abort with formatted context (the crate's
// lib.rs charter: a scenario whose rig broke is a failed test,
// never a production failure path).
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;
use volvisor_campaign::tier_v;

// ------------------------------------------------------------ discovery

/// One committed evidence bundle: a date-labeled host-run directory
/// (`docs/evidence/tier-v/<date>-<host>/`) with its `records/` and
/// its `README.md`.
struct Bundle {
    /// The bundle's directory (for failure messages).
    dir: PathBuf,
    /// The parsed records, keyed by scenario name.
    records: BTreeMap<String, Value>,
    /// The bundle's README text.
    readme: String,
}

/// The workspace root: cargo runs test binaries from the crate
/// directory, so the tree root is two levels up from the manifest.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("the workspace root canonicalizes")
}

/// Discover every committed bundle: a `tier-v` subdirectory that
/// holds a `records/` directory. The committed tree is part of the
/// repository (the nearline §10 ledger's "the first recorded host
/// run is committed under `docs/evidence/tier-v/`" claim depends on
/// it), so a fresh checkout finds it — an empty discovery is a
/// failure, never a pass.
fn bundles() -> Vec<Bundle> {
    let root = workspace_root()
        .join("docs")
        .join("evidence")
        .join("tier-v");
    let mut found = Vec::new();
    for entry in
        std::fs::read_dir(&root).unwrap_or_else(|error| panic!("read {}: {error}", root.display()))
    {
        let dir = entry
            .unwrap_or_else(|error| panic!("enumerate {}: {error}", root.display()))
            .path();
        let records_dir = dir.join("records");
        if !records_dir.is_dir() {
            continue;
        }
        let mut records = BTreeMap::new();
        for record_entry in std::fs::read_dir(&records_dir)
            .unwrap_or_else(|error| panic!("read {}: {error}", records_dir.display()))
        {
            let path = record_entry
                .unwrap_or_else(|error| panic!("enumerate {}: {error}", records_dir.display()))
                .path();
            if path.extension().is_none_or(|extension| extension != "json") {
                continue;
            }
            let body = std::fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
            let record: Value = serde_json::from_str(&body)
                .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()));
            let scenario = record["scenario"]
                .as_str()
                .unwrap_or_else(|| panic!("{}: the record names its scenario", path.display()))
                .to_owned();
            assert!(
                records.insert(scenario, record).is_none(),
                "{}: two records claim the same scenario",
                path.display()
            );
        }
        let readme_path = dir.join("README.md");
        let readme = std::fs::read_to_string(&readme_path)
            .unwrap_or_else(|error| panic!("read {}: {error}", readme_path.display()));
        found.push(Bundle {
            dir,
            records,
            readme,
        });
    }
    found
}

// ------------------------------------------------------------ the checks

/// A 64-hex SHA256 digest.
fn is_sha256(text: &str) -> bool {
    text.len() == 64 && text.chars().all(|c| c.is_ascii_hexdigit())
}

/// Every committed bundle carries the tier's full record shape:
/// exactly the tier's scenario set, tier `"V"`, the outcome
/// vocabulary (a skip observes nothing; a run names its VMM), and
/// well-formed binary stamps on every run record.
#[test]
fn every_committed_record_carries_the_tier_v_shape() {
    let found = bundles();
    assert!(
        !found.is_empty(),
        "the committed evidence tree exists (the nearline §10 ledger claims it)"
    );
    for bundle in &found {
        let mut expected: Vec<String> = tier_v::SCENARIOS.iter().map(tier_v::record_name).collect();
        expected.sort();
        let mut actual: Vec<String> = bundle.records.keys().cloned().collect();
        actual.sort();
        assert_eq!(
            actual,
            expected,
            "bundle {} carries exactly the tier's four scenario records",
            bundle.dir.display()
        );
        for (scenario, record) in &bundle.records {
            assert_eq!(
                record["tier"], "V",
                "{scenario}: the committed record is a Tier V record"
            );
            let outcome = record["outcome"]
                .as_str()
                .unwrap_or_else(|| panic!("{scenario}: the outcome is a string"));
            if outcome == "skipped" {
                // The skip shape: a gate statement observes nothing
                // about the environment and documents what would run.
                assert!(
                    record["components"].is_null(),
                    "{scenario}: a skip observes nothing (components null)"
                );
                assert!(
                    record["reason"]
                        .as_str()
                        .is_some_and(|reason| !reason.is_empty()),
                    "{scenario}: a skip carries its reason"
                );
                assert!(
                    record["would_run"]
                        .as_str()
                        .is_some_and(|would_run| !would_run.is_empty()),
                    "{scenario}: a skip documents its body"
                );
            } else {
                // The run shape: a non-empty observed outcome, no
                // reason field, and the binary stamps a run record
                // is not evidence without (ADR-0010; the summary's
                // CG5 coherence clause is the render-side twin).
                assert!(
                    !outcome.is_empty(),
                    "{scenario}: a run outcome is non-empty"
                );
                assert!(
                    record["reason"].is_null(),
                    "{scenario}: a run record carries no reason field"
                );
                for binary in ["cloud_hypervisor", "ch_remote"] {
                    let stamp = &record["components"]["vmm"][binary];
                    let version = stamp["version"]
                        .as_str()
                        .unwrap_or_else(|| panic!("{scenario}: {binary} carries its version"));
                    assert!(
                        !version.is_empty(),
                        "{scenario}: the {binary} version is non-empty"
                    );
                    let sha256 = stamp["sha256"]
                        .as_str()
                        .unwrap_or_else(|| panic!("{scenario}: {binary} carries its SHA256"));
                    assert!(
                        is_sha256(sha256),
                        "{scenario}: the {binary} SHA256 is 64 hex: {sha256}"
                    );
                    assert!(
                        stamp["path"]
                            .as_str()
                            .is_some_and(|path| Path::new(path).is_absolute()),
                        "{scenario}: the {binary} stamp names an absolute path"
                    );
                }
                assert!(
                    record["duration_ms"].as_u64().is_some(),
                    "{scenario}: a run carries its duration"
                );
                let commit = record["commit"]
                    .as_str()
                    .unwrap_or_else(|| panic!("{scenario}: a run names its commit"));
                assert!(
                    commit.len() == 40 && commit.chars().all(|c| c.is_ascii_hexdigit()),
                    "{scenario}: the commit is a full git hash: {commit}"
                );
            }
        }
    }
}

/// Every bundle's README agrees with its records wherever the
/// agreement is mechanical: the per-scenario table names every
/// scenario exactly once, its outcome word matches the record's
/// outcome, its duration is the record's `duration_ms`, and the
/// table has exactly as many scenario rows as the bundle has
/// records. This is the pin that keeps a README's reading from
/// drifting from its own authority (review round 1, NIT-1).
#[test]
fn the_readme_table_agrees_with_the_records() {
    for bundle in &bundles() {
        for scenario in tier_v::SCENARIOS {
            let name = tier_v::record_name(scenario);
            let record = bundle
                .records
                .get(&name)
                .unwrap_or_else(|| panic!("{name}: the record exists"));
            // Exactly one table row names the scenario (the row
            // convention: "| `<name>` | <outcome word> (<N> ms) | ...").
            let prefix = format!("| `{}`", scenario.name);
            let rows: Vec<&str> = bundle
                .readme
                .lines()
                .filter(|line| line.starts_with(&prefix))
                .collect();
            assert_eq!(
                rows.len(),
                1,
                "{}: the README's table names {} exactly once",
                bundle.dir.display(),
                scenario.name
            );
            let cells: Vec<&str> = rows[0].split('|').map(str::trim).collect();
            let outcome_cell = cells[2];
            let outcome = record["outcome"].as_str().expect("the record's outcome");
            let outcome_word = outcome.split(':').next().unwrap_or_default();
            let readme_word = outcome_cell.split(' ').next().unwrap_or_default();
            assert_eq!(
                readme_word, outcome_word,
                "{name}: the README's outcome word is the record's ({outcome})"
            );
            let duration_ms = record["duration_ms"].as_u64().expect("the duration");
            assert!(
                outcome_cell.contains(&format!("({duration_ms} ms)")),
                "{name}: the README's duration cell carries the record's duration_ms \
                 ({duration_ms} ms): {outcome_cell}"
            );
        }
        // The table's scenario-row count matches the record count —
        // no row without a record, no record without a row.
        let scenario_rows = bundle
            .readme
            .lines()
            .filter(|line| line.starts_with("| `"))
            .count();
        assert_eq!(
            scenario_rows,
            bundle.records.len(),
            "{}: the README's table has exactly one row per record",
            bundle.dir.display()
        );
    }
}
