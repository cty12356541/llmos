//! W36-P8 scheduler scale probe (W31-G §8.2.7 / B-PLAN-001 §11.5):
//! the two-tier materialization scheduler itself, not the `TaskNode`
//! metadata probe. Default-suite smoke always runs; the 10K ignored
//! cell is the re-runnable scale measurement.
//!
//! ```sh
//! cargo test -p nlos-plan --test scheduler_scale_probe -- --nocapture
//! cargo test -p nlos-plan --test scheduler_scale_probe -- --ignored --nocapture
//! ```

use std::convert::Infallible;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nlos_plan::{
    AdmissionConsult, AdmissionConsultOutcome, ApplyPlanRevisionRequest, MaterializationAdmission,
    MaterializationScheduler, PlanNodeDeclaration, PlanNodeKind, SqlitePlanAuthority,
};
use nlos_types::IdempotencyKey;

const SMOKE_COUNT: u64 = 48;
const SCALE_COUNT: u64 = 10_000;
/// Select p95 must stay well under this ceiling (debug/test, single
/// platform). The probe records wall time; the only assertion is the
/// absolute ceiling plus "every ready node is selected".
const SELECT_CEILING: Duration = Duration::from_secs(5);

static NEXT: AtomicU64 = AtomicU64::new(1);

struct Root(std::path::PathBuf);

impl Root {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "nlos-plan-sched-scale-{label}-{}-{nonce}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("create fixture directory");
        Self(path)
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn node(index: u64) -> PlanNodeDeclaration {
    let mut key = [0_u8; 16];
    key[8..].copy_from_slice(&index.to_be_bytes());
    PlanNodeDeclaration {
        node_key: key,
        kind: PlanNodeKind::AgentRole,
        binding_digest: [0x11; 32],
        dependency_keys: Vec::new(),
        input_selectors_digest: [0x22; 32],
        output_contract_digest: [0x33; 32],
        policy_digest: [0x44; 32],
        resource_ceiling_digest: [0x55; 32],
        conditions: None,
    }
}

struct AdmitAll;

impl AdmissionConsult for AdmitAll {
    type Error = Infallible;

    fn consult_materialization(
        &self,
        other_declared_task_nodes: u64,
    ) -> Result<AdmissionConsultOutcome, Infallible> {
        Ok(AdmissionConsultOutcome::Admitted(
            MaterializationAdmission {
                profile_id: "sched-scale-admit".to_string(),
                projected_task_nodes: other_declared_task_nodes + 1,
                projected_active_working_set: 1,
            },
        ))
    }
}

fn run_scheduler_scale_cell(count: u64, print: bool) {
    let root = Root::new(&format!("n{count}"));
    let authority = SqlitePlanAuthority::open(root.0.join("plan.sqlite3")).expect("open plan");
    let apply_started = Instant::now();
    let plan_id = authority
        .apply_plan_revision_ungated(ApplyPlanRevisionRequest {
            plan_id: None,
            nodes: (0..count).map(node).collect(),
            idempotency_key: IdempotencyKey::from_bytes([0x51; 16]),
            applied_at_ms: 1_000,
        })
        .expect("apply independent nodes")
        .receipt()
        .plan_id;
    let apply_elapsed = apply_started.elapsed();

    let mut scheduler = MaterializationScheduler::new(count);
    let select_started = Instant::now();
    let report = scheduler
        .select(&authority, plan_id)
        .expect("select at scale");
    let select_elapsed = select_started.elapsed();
    assert_eq!(report.selections.len() as u64, count);
    assert!(report.skips.is_empty());
    assert!(
        select_elapsed < SELECT_CEILING,
        "select {count} nodes took {select_elapsed:?} (>= {SELECT_CEILING:?})"
    );

    let drive_started = Instant::now();
    let summary = scheduler
        .drive(&authority, &report, &AdmitAll, 2_000)
        .expect("drive at scale");
    let drive_elapsed = drive_started.elapsed();
    assert_eq!(summary.selected, count);
    assert_eq!(summary.approved, count);
    assert_eq!(summary.rejected, 0);
    assert_eq!(summary.skipped, 0);

    if print {
        eprintln!(
            "W36-P8 scheduler scale probe (single platform): \
             nodes={count} apply_total={apply_elapsed:?} \
             select_total={select_elapsed:?} drive_total={drive_elapsed:?} \
             selected={} approved={}",
            summary.selected, summary.approved
        );
    }
}

#[test]
fn scheduler_scale_smoke_selects_and_drives_ready_fifo() {
    run_scheduler_scale_cell(SMOKE_COUNT, false);
}

#[test]
#[ignore = "explicit W36-P8 scheduler 10K scale probe (B-PLAN-001 §11.5)"]
fn ten_thousand_nodes_scheduler_select_and_drive() {
    run_scheduler_scale_cell(SCALE_COUNT, true);
}
