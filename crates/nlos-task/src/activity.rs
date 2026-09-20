//! Task-activity read surface for cross-authority lifecycle gates
//! (W27-D / B-APPLICATION-003 §W27-D).
//!
//! [`SqliteTaskAuthority::inspect_outstanding_task_count`] answers one
//! question for callers outside the task domain — most importantly the
//! uninstall/rollback activity gate in `nlos-application` — without
//! exposing any task internals: of the named Task identities, how many
//! are still outstanding (a durable `tasks` row in the non-terminal
//! `Active` state)? A named Task with no durable row is not activity (it
//! was never created, or it lives in another authority); a Task that
//! reached a terminal state (`Cancelled`) is not activity either. A
//! stored `task_state` code this build cannot decode is a typed
//! fail-closed error, never a silent skip.

use crate::TaskStoreError;
use crate::model::TaskState;
use crate::store::{SqlRead, SqliteTaskAuthority};
use nlos_types::TaskId;

impl SqliteTaskAuthority {
    /// Counts the distinct named Tasks that are still outstanding.
    ///
    /// Duplicates in `task_ids` are collapsed (one Task identity counts
    /// once); a Task with no durable row contributes zero, and a Task
    /// whose durable state is terminal (`Cancelled`) contributes zero.
    ///
    /// # Errors
    ///
    /// Returns [`TaskStoreError::CorruptRecord`] for an undecodable
    /// stored task state, or a storage error.
    pub fn inspect_outstanding_task_count(
        &self,
        task_ids: &[TaskId],
    ) -> Result<u64, TaskStoreError> {
        let connection = self.lock_connection()?;
        let mut distinct = task_ids.to_vec();
        distinct.sort_unstable();
        distinct.dedup();
        let mut statement =
            connection.prepare_statement("SELECT task_state FROM tasks WHERE task_id = ?1")?;
        let mut outstanding = 0_u64;
        for task_id in distinct {
            let mut rows = statement.query([task_id.as_bytes().as_slice()])?;
            if let Some(row) = rows.next()? {
                let code: i64 = row.get(0)?;
                if TaskState::from_code(code)? == TaskState::Active {
                    outstanding += 1;
                }
            }
        }
        Ok(outstanding)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CancelRequest, TaskRegistrationDecision, TaskSpec};
    use nlos_types::{Generation, IdempotencyKey};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct TempDatabase {
        path: std::path::PathBuf,
    }

    impl TempDatabase {
        fn new(name: &str) -> Self {
            let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "nlos-task-activity-{name}-{}-{sequence}.sqlite3",
                std::process::id()
            ));
            Self { path }
        }

        fn open(&self) -> SqliteTaskAuthority {
            SqliteTaskAuthority::open(&self.path).expect("open task authority")
        }
    }

    impl Drop for TempDatabase {
        fn drop(&mut self) {
            for path in [
                self.path.clone(),
                std::path::PathBuf::from(format!("{}-wal", self.path.display())),
                std::path::PathBuf::from(format!("{}-shm", self.path.display())),
            ] {
                let _ = std::fs::remove_file(path);
            }
        }
    }

    fn registered(authority: &SqliteTaskAuthority, seed: u8) -> TaskId {
        let task_id = TaskId::from_bytes([seed; 16]);
        match authority.register_task(TaskSpec {
            task_id,
            task_generation: Generation::INITIAL,
            registered_at_ms: 1_000,
        }) {
            Ok(TaskRegistrationDecision::Created(id) | TaskRegistrationDecision::Existing(id)) => {
                assert_eq!(id, task_id);
            }
            Err(error) => panic!("register task {seed:#x}: {error}"),
        }
        task_id
    }

    fn cancelled(authority: &SqliteTaskAuthority, seed: u8) {
        let decision = authority
            .cancel_task(CancelRequest {
                task_id: TaskId::from_bytes([seed; 16]),
                idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(0x50); 16]),
                requested_at_ms: 2_000,
            })
            .expect("cancel task");
        assert!(matches!(decision, crate::CancelDecision::Applied { .. }));
    }

    #[test]
    fn empty_and_unknown_ids_count_zero() {
        let database = TempDatabase::new("empty-unknown");
        let authority = database.open();
        let unknown = TaskId::from_bytes([0xEE; 16]);
        assert_eq!(
            authority
                .inspect_outstanding_task_count(&[])
                .expect("empty"),
            0
        );
        assert_eq!(
            authority
                .inspect_outstanding_task_count(&[unknown])
                .expect("unknown id"),
            0
        );
    }

    #[test]
    fn counts_distinct_active_tasks_and_honors_terminal_states() {
        let database = TempDatabase::new("distinct-active");
        let authority = database.open();
        let active_a = registered(&authority, 0xA1);
        let cancelled_b = registered(&authority, 0xB2);
        cancelled(&authority, 0xB2);
        let active_c = registered(&authority, 0xC3);

        assert_eq!(
            authority
                .inspect_outstanding_task_count(&[active_a, cancelled_b, active_c])
                .expect("mixed states"),
            2
        );
        assert_eq!(
            authority
                .inspect_outstanding_task_count(&[active_a, active_a, active_a])
                .expect("duplicates collapse"),
            1
        );
        assert_eq!(
            authority
                .inspect_outstanding_task_count(&[cancelled_b])
                .expect("terminal task is not activity"),
            0
        );
    }

    #[test]
    fn fails_closed_on_corrupt_task_state() {
        let database = TempDatabase::new("corrupt-state");
        let authority = database.open();
        let task_id = registered(&authority, 0xD4);

        let raw = rusqlite::Connection::open(&database.path).expect("open raw tamper");
        raw.execute(
            "UPDATE tasks SET task_state = 7 WHERE task_id = ?1",
            [task_id.as_bytes().as_slice()],
        )
        .expect("tamper task state");

        let error = authority
            .inspect_outstanding_task_count(&[task_id])
            .expect_err("corrupt state must fail closed");
        assert!(
            matches!(error, TaskStoreError::CorruptRecord(reason) if reason.contains("task state")),
            "unexpected error: {error:?}"
        );
    }
}
