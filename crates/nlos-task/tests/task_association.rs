//! ADR-0016 决定 3 `TaskSpec` association fields acceptance: one additive
//! migration (schema v44) adds `application_id` and the plan revision
//! reference (`plan_id` + `revision`) to the Task declaration path.
//!
//! 决定 3 stores **references only** — runtime verification stays at the
//! materialization/permit boundaries per ADR-0013 verify-then-commit (W30-D
//! sinks the association into those gates; this file pins the storage
//! semantics):
//!
//! 1. association round-trips through `register_task`/`inspect_task` and
//!    survives `TaskHead`-mutating flows (cancel rewrites the task row),
//! 2. the association of a registered task is declaration identity:
//!    idempotent replay must repeat it exactly; a mismatched association
//!    (including binding an association onto a legacy NULL row) fails
//!    closed,
//! 3. pre-v44 rows migrate with NULL association and read back as `None`,
//!    never an invented reference,
//! 4. the legacy unassociated registration path is unchanged.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_task::{
    CancelRequest, SqliteTaskAuthority, TaskPlanRevisionRef, TaskRegistrationDecision, TaskSpec,
    TaskStoreError,
};
use nlos_types::{ApplicationId, Generation, IdempotencyKey, TaskId, TaskPlanId};
use rusqlite::Connection;

static NEXT_DATABASE: AtomicU64 = AtomicU64::new(0);

struct TestDatabase {
    path: PathBuf,
}

impl TestDatabase {
    fn new(name: &str) -> Self {
        let sequence = NEXT_DATABASE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "nlos-task-association-{name}-{}-{sequence}.sqlite3",
            std::process::id()
        ));
        Self { path }
    }

    fn open(&self) -> SqliteTaskAuthority {
        SqliteTaskAuthority::open(&self.path).expect("open task authority")
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        for path in [
            self.path.clone(),
            suffix_path(&self.path, "-wal"),
            suffix_path(&self.path, "-shm"),
        ] {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("remove test database: {error}"),
            }
        }
    }
}

fn suffix_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn id_bytes(domain: u8, index: u64) -> [u8; 16] {
    let mut bytes = [domain; 16];
    bytes[8..].copy_from_slice(&index.to_be_bytes());
    bytes
}

fn task_id(index: u64) -> TaskId {
    TaskId::from_bytes(id_bytes(0x01, index))
}

fn application_id(index: u64) -> ApplicationId {
    ApplicationId::from_bytes(id_bytes(0x40, index))
}

fn plan_id(index: u64) -> TaskPlanId {
    TaskPlanId::from_bytes(id_bytes(0x50, index))
}

fn unassociated_spec(index: u64) -> TaskSpec {
    TaskSpec {
        task_id: task_id(index),
        task_generation: Generation::INITIAL,
        registered_at_ms: 1_000,
        application_id: None,
        plan_revision: None,
    }
}

fn associated_spec(index: u64, application: u64, plan: u64, revision: u64) -> TaskSpec {
    TaskSpec {
        application_id: Some(application_id(application)),
        plan_revision: Some(TaskPlanRevisionRef {
            plan_id: plan_id(plan),
            revision,
        }),
        ..unassociated_spec(index)
    }
}

#[test]
fn association_round_trips_and_survives_head_mutation() {
    let database = TestDatabase::new("round-trip");
    let authority = database.open();

    let full = associated_spec(0, 7, 9, 12);
    assert_eq!(
        authority.register_task(full).expect("register associated"),
        TaskRegistrationDecision::Created(task_id(0))
    );

    // Application-only association: the two association fields are
    // independently optional (决定 3 adds both in one migration).
    let app_only = TaskSpec {
        application_id: Some(application_id(8)),
        plan_revision: None,
        ..unassociated_spec(1)
    };
    assert_eq!(
        authority
            .register_task(app_only)
            .expect("register application-only association"),
        TaskRegistrationDecision::Created(task_id(1))
    );

    let readback = authority.inspect_task(task_id(0)).expect("inspect task");
    assert_eq!(readback.application_id, Some(application_id(7)));
    assert_eq!(
        readback.plan_revision,
        Some(TaskPlanRevisionRef {
            plan_id: plan_id(9),
            revision: 12,
        })
    );
    let app_only_readback = authority.inspect_task(task_id(1)).expect("inspect task");
    assert_eq!(app_only_readback.application_id, Some(application_id(8)));
    assert_eq!(app_only_readback.plan_revision, None);

    // Idempotent replay repeats the exact association.
    assert_eq!(
        authority.register_task(full).expect("replay associated"),
        TaskRegistrationDecision::Existing(task_id(0))
    );

    // A `TaskHead`-mutating flow rewrites the task row; the association
    // must survive it byte-equal (the v44 immutability trigger refuses
    // association rewrites on exactly this path).
    authority
        .cancel_task(CancelRequest {
            task_id: task_id(0),
            idempotency_key: IdempotencyKey::from_bytes(id_bytes(0xc0, 0)),
            requested_at_ms: 2_000,
        })
        .expect("cancel task");
    let after_cancel = authority
        .inspect_task(task_id(0))
        .expect("inspect after cancel");
    assert_eq!(after_cancel.application_id, Some(application_id(7)));
    assert_eq!(
        after_cancel.plan_revision,
        Some(TaskPlanRevisionRef {
            plan_id: plan_id(9),
            revision: 12,
        })
    );
}

#[test]
fn association_mismatch_on_replay_fails_closed() {
    let database = TestDatabase::new("mismatch");
    let authority = database.open();
    let spec = associated_spec(0, 7, 9, 12);
    assert_eq!(
        authority.register_task(spec).expect("register"),
        TaskRegistrationDecision::Created(task_id(0))
    );

    let mismatched_application = TaskSpec {
        application_id: Some(application_id(99)),
        ..spec
    };
    assert!(matches!(
        authority.register_task(mismatched_application),
        Err(TaskStoreError::TaskAssociationConflict { task_id: conflict })
        if conflict == task_id(0)
    ));

    let mismatched_plan = TaskSpec {
        plan_revision: Some(TaskPlanRevisionRef {
            plan_id: plan_id(99),
            revision: 12,
        }),
        ..spec
    };
    assert!(matches!(
        authority.register_task(mismatched_plan),
        Err(TaskStoreError::TaskAssociationConflict { .. })
    ));

    let mismatched_revision = TaskSpec {
        plan_revision: Some(TaskPlanRevisionRef {
            plan_id: plan_id(9),
            revision: 13,
        }),
        ..spec
    };
    assert!(matches!(
        authority.register_task(mismatched_revision),
        Err(TaskStoreError::TaskAssociationConflict { .. })
    ));

    let dropped_association = TaskSpec {
        application_id: None,
        plan_revision: None,
        ..spec
    };
    assert!(matches!(
        authority.register_task(dropped_association),
        Err(TaskStoreError::TaskAssociationConflict { .. })
    ));

    // Generation mismatch keeps its pre-v44 meaning even when the
    // association also differs.
    let generation_first = TaskSpec {
        task_generation: Generation::new(
            std::num::NonZeroU64::new(2).expect("non-zero generation"),
        ),
        application_id: Some(application_id(99)),
        ..spec
    };
    assert!(matches!(
        authority.register_task(generation_first),
        Err(TaskStoreError::DuplicateTask)
    ));

    // The durable association is unchanged after every rejected replay.
    let readback = authority.inspect_task(task_id(0)).expect("inspect");
    assert_eq!(readback.application_id, Some(application_id(7)));
}

#[test]
fn legacy_v43_rows_migrate_with_null_association() {
    let database = TestDatabase::new("legacy-v43");
    let authority = database.open();
    assert_eq!(
        authority
            .register_task(unassociated_spec(0))
            .expect("register legacy-shaped row"),
        TaskRegistrationDecision::Created(task_id(0))
    );
    drop(authority);

    // Rewind to the pre-v44 shape: no association columns, no trigger.
    let raw = Connection::open(&database.path).expect("raw legacy database");
    raw.execute_batch(
        "DROP TRIGGER task_association_immutable;
         ALTER TABLE tasks DROP COLUMN application_id;
         ALTER TABLE tasks DROP COLUMN plan_id;
         ALTER TABLE tasks DROP COLUMN plan_revision;
         PRAGMA user_version = 43;",
    )
    .expect("construct v43 legacy schema");
    drop(raw);

    // Reopening migrates v43 → v44 additively: the columns return as
    // nullable, and the existing row is backfilled NULL, not fabricated.
    let authority = database.open();
    let readback = authority
        .inspect_task(task_id(0))
        .expect("inspect migrated row");
    assert_eq!(readback.application_id, None);
    assert_eq!(readback.plan_revision, None);

    let raw = Connection::open(&database.path).expect("raw migrated database");
    let version: i64 = raw
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .expect("read migrated version");
    assert_eq!(version, 44);
    let association_columns: i64 = raw
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('tasks')
             WHERE name IN ('application_id', 'plan_id', 'plan_revision')
               AND \"notnull\" = 0",
            [],
            |row| row.get(0),
        )
        .expect("inspect association columns");
    assert_eq!(
        association_columns, 3,
        "association columns must exist and stay nullable for legacy rows"
    );
    let trigger_present: i64 = raw
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type='trigger' AND name='task_association_immutable'",
            [],
            |row| row.get(0),
        )
        .expect("inspect immutability trigger");
    assert_eq!(trigger_present, 1);
    drop(raw);

    // A legacy NULL row cannot be re-bound to an association by replay:
    // the association is declaration identity from v44 on.
    assert!(matches!(
        authority.register_task(associated_spec(0, 7, 9, 12)),
        Err(TaskStoreError::TaskAssociationConflict { task_id: conflict })
        if conflict == task_id(0)
    ));
    // Legacy replay without association keeps the pre-v44 behavior.
    assert_eq!(
        authority
            .register_task(unassociated_spec(0))
            .expect("legacy replay"),
        TaskRegistrationDecision::Existing(task_id(0))
    );
}

#[test]
fn unassociated_registration_path_is_unchanged() {
    let database = TestDatabase::new("unassociated");
    let authority = database.open();

    assert_eq!(
        authority
            .register_task(unassociated_spec(0))
            .expect("register without association"),
        TaskRegistrationDecision::Created(task_id(0))
    );
    assert_eq!(
        authority
            .register_task(unassociated_spec(0))
            .expect("replay without association"),
        TaskRegistrationDecision::Existing(task_id(0))
    );
    let readback = authority.inspect_task(task_id(0)).expect("inspect");
    assert_eq!(readback.application_id, None);
    assert_eq!(readback.plan_revision, None);
}
