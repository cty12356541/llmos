//! Linear `SQLite` schema migration chain (v1 → v39) for the durable
//! `TaskAuthority`.
//!
//! Every `migrate_vN` advances `user_version` by exactly one step, committed
//! in a single `BEGIN IMMEDIATE` transaction so a failure anywhere rolls
//! back to a complete v(N-1) database, never a half-migrated one. The
//! explicit linear chain (no loops, no table dispatch) is deliberately
//! easier to audit. The chain is executed only by
//! `SqliteTaskAuthority::open_with_vfs`.

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

use crate::TaskStoreError;

pub(crate) fn migrate_v1(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(SCHEMA_V1_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v1 → v2 is purely additive (new effect-plane tables + `user_version`),
/// committed in one transaction: a failure anywhere rolls back to a
/// complete v1 database, never a half-migrated one.
pub(crate) fn migrate_v2(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(crate::effect::SCHEMA_V2_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v2 → v3 is purely additive (effect history + quarantine/adoption/
/// reconcile receipts + monotonic sequences + `user_version`), committed
/// in one transaction: a failure anywhere rolls back to a complete v2
/// database, never a half-migrated one.
pub(crate) fn migrate_v3(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(crate::effect::SCHEMA_V3_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v3 → v4 is purely additive (`TaskGroup` plane: groups, members,
/// admission/removal receipts, group cancels, attempt group bindings +
/// `user_version`), committed in one transaction: a failure anywhere
/// rolls back to a complete v3 database, never a half-migrated one.
pub(crate) fn migrate_v4(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(crate::group::SCHEMA_V4_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v4 → v5 is purely additive: nullable group-binding columns are added to
/// permits and receipts. Existing ungrouped/v1-v4 rows decode as `None`;
/// new grouped permits persist all four fields and copy them verbatim into
/// their terminal receipt.
pub(crate) fn migrate_v5(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(SCHEMA_V5_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v5 → v6 adds the immutable Artifact publication plan bound to one
/// outstanding permit. It records intent only; publication authorization
/// and receipt consumption are later state transitions.
pub(crate) fn migrate_v6(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(crate::commit::SCHEMA_V6_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v6 → v7 adds immutable nested Artifact publication receipt evidence.
pub(crate) fn migrate_v7(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(crate::commit::SCHEMA_V7_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v7 → v8 adds the mutable per-plan recovery scheduling and escalation
/// ledger. Canonical plan/publication/Task receipts remain immutable.
pub(crate) fn migrate_v8(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(crate::recovery::SCHEMA_V8_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v8 → v9 adds immutable acknowledgements for individual durable
/// escalation instances. Recovery scheduling remains in the mutable ledger.
pub(crate) fn migrate_v9(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(crate::recovery::SCHEMA_V9_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v9 → v10 adds immutable snapshot receipts, their ordered authority
/// checkpoint receipt set, and an optional binding on attempts. Existing
/// attempts remain explicitly legacy/unreceipted rather than receiving
/// invented proof during migration.
pub(crate) fn migrate_v10(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(SCHEMA_V10_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v10 → v11 adds authority-assigned `TaskStore` participant identity,
/// versioned participant registries/receipts, and permit-time registry
/// generation/root bindings. Existing permits remain explicitly unbound.
pub(crate) fn migrate_v11(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let table_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type='table' AND name IN (
            'task_authority_identity', 'task_participant_registries',
            'task_participants', 'task_participant_registry_receipts'
         )",
        [],
        |row| row.get(0),
    )?;
    let trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type='trigger' AND (
            name LIKE 'task_participant_registry%'
            OR name LIKE 'task_participants_%'
            OR name LIKE 'task_authority_identity_%'
         )",
        [],
        |row| row.get(0),
    )?;
    let mut has_generation = false;
    let mut has_root = false;
    {
        let mut statement = connection.prepare("PRAGMA table_info(commit_permits)")?;
        let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
        for column in columns {
            match column?.as_str() {
                "participant_registry_generation" => has_generation = true,
                "participant_registry_root" => has_root = true,
                _ => {}
            }
        }
    }
    if table_count == 4 && trigger_count == 8 && has_generation && has_root {
        connection.pragma_update(None, "user_version", 11)?;
        return Ok(());
    }
    if table_count != 0 || trigger_count != 0 || has_generation || has_root {
        return Err(TaskStoreError::CorruptRecord(
            "partial participant registry schema",
        ));
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(crate::participant::SCHEMA_V11_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v11 → v12 copies the frozen participant registry binding into every new
/// `EffectPermit` and permit-backed Task receipt. Existing rows remain
/// explicitly unbound rather than receiving invented authority evidence.
pub(crate) fn migrate_v12(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let mut present = 0usize;
    for (table, expected) in [
        (
            "effect_permits",
            [
                "participant_registry_generation",
                "participant_registry_root",
            ],
        ),
        (
            "task_receipts",
            [
                "participant_registry_generation",
                "participant_registry_root",
            ],
        ),
    ] {
        let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
        let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
        for column in columns {
            if expected.contains(&column?.as_str()) {
                present += 1;
            }
        }
    }
    let trigger_present: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM sqlite_master
            WHERE type='trigger' AND name='effect_permit_participant_binding_immutable'
         )",
        [],
        |row| row.get(0),
    )?;
    if present == 4 && trigger_present {
        connection.pragma_update(None, "user_version", 12)?;
        return Ok(());
    }
    if present != 0 || trigger_present {
        return Err(TaskStoreError::CorruptRecord(
            "partial participant binding propagation schema",
        ));
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(SCHEMA_V12_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v12 → v13 adds the immutable, authority-derived snapshot/read-set
/// `TaskWriteSet` seal. Existing tasks receive no invented write-set rows.
pub(crate) fn migrate_v13(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let table_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN
            ('task_write_sets', 'task_write_set_artifact_reads')",
        [],
        |row| row.get(0),
    )?;
    let trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name IN
            ('task_write_set_is_immutable', 'task_write_set_is_immutable_delete',
             'task_write_set_artifact_read_is_immutable',
             'task_write_set_artifact_read_is_immutable_delete')",
        [],
        |row| row.get(0),
    )?;
    if table_count == 2 && trigger_count == 4 {
        connection.pragma_update(None, "user_version", 13)?;
        return Ok(());
    }
    if table_count != 0 || trigger_count != 0 {
        return Err(TaskStoreError::CorruptRecord("partial TaskWriteSet schema"));
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(SCHEMA_V13_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v13 → v14 adds the immutable owner-verified Process binding child for a
/// `TaskWriteSet`. Existing seals remain valid and explicitly have no Process
/// binding; no caller-supplied execution identity is invented during upgrade.
pub(crate) fn migrate_v14(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let table_present: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM sqlite_master
            WHERE type='table' AND name='task_write_set_process_bindings'
         )",
        [],
        |row| row.get(0),
    )?;
    let trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name IN
            ('task_write_set_process_binding_is_immutable',
             'task_write_set_process_binding_is_immutable_delete')",
        [],
        |row| row.get(0),
    )?;
    if table_present && trigger_count == 2 {
        connection.pragma_update(None, "user_version", 14)?;
        return Ok(());
    }
    if table_present || trigger_count != 0 {
        return Err(TaskStoreError::CorruptRecord(
            "partial TaskWriteSet Process binding schema",
        ));
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(SCHEMA_V14_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v14 → v15 widens the participant type check to include the Process
/// binding endpoint. The immutable participant rows are copied byte-for-byte;
/// no registry generation or root is rewritten.
pub(crate) fn migrate_v15(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let table_sql: Option<String> = connection
        .query_row(
            "SELECT sql FROM sqlite_master
             WHERE type='table' AND name='task_participants'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let Some(table_sql) = table_sql else {
        return Err(TaskStoreError::CorruptRecord(
            "missing participant table during v15 migration",
        ));
    };
    let trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name IN
            ('task_participants_immutable_update',
             'task_participants_immutable_delete')",
        [],
        |row| row.get(0),
    )?;
    if (table_sql.contains("BETWEEN 1 AND 7") || table_sql.contains("BETWEEN 1 AND 8"))
        && trigger_count == 2
    {
        connection.pragma_update(None, "user_version", 15)?;
        return Ok(());
    }
    if table_sql.contains("BETWEEN 1 AND 7") || table_sql.contains("BETWEEN 1 AND 8") {
        return Err(TaskStoreError::CorruptRecord(
            "partial Process participant type migration",
        ));
    }
    if !table_sql.contains("BETWEEN 1 AND 6") {
        return Err(TaskStoreError::CorruptRecord(
            "unexpected participant type constraint",
        ));
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(SCHEMA_V15_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v15 → v16 adds immutable Semantic read and Resource Reservation children
/// for the authority-verified `TaskWriteSet` slice.
pub(crate) fn migrate_v16(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let table_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN
            ('task_write_set_semantic_reads', 'task_write_set_resource_reservations')",
        [],
        |row| row.get(0),
    )?;
    let trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name IN
            ('task_write_set_semantic_read_is_immutable',
             'task_write_set_semantic_read_is_immutable_delete',
             'task_write_set_resource_reservation_is_immutable',
             'task_write_set_resource_reservation_is_immutable_delete')",
        [],
        |row| row.get(0),
    )?;
    let root_column_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('task_write_sets')
         WHERE name IN ('semantic_read_set_root', 'resource_reservation_set_root')",
        [],
        |row| row.get(0),
    )?;
    if table_count == 2 && trigger_count == 4 && root_column_count == 2 {
        connection.pragma_update(None, "user_version", 16)?;
        return Ok(());
    }
    if table_count != 0 || trigger_count != 0 || root_column_count != 0 {
        return Err(TaskStoreError::CorruptRecord(
            "partial Semantic/Resource TaskWriteSet schema",
        ));
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(SCHEMA_V16_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v16 → v17 adds the immutable planned-effect declaration to each verified
/// `TaskWriteSet`. Existing rows keep a zero effect root and no invented
/// planned slots, preserving their v1/v2 write-set roots.
pub(crate) fn migrate_v17(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let table_present: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM sqlite_master
            WHERE type='table' AND name='task_write_set_planned_effects'
         )",
        [],
        |row| row.get(0),
    )?;
    let trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name IN
            ('task_write_set_planned_effect_is_immutable',
             'task_write_set_planned_effect_is_immutable_delete')",
        [],
        |row| row.get(0),
    )?;
    let root_column_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('task_write_sets')
         WHERE name = 'effect_set_root'",
        [],
        |row| row.get(0),
    )?;
    if table_present && trigger_count == 2 && root_column_count == 1 {
        connection.pragma_update(None, "user_version", 17)?;
        return Ok(());
    }
    if table_present || trigger_count != 0 || root_column_count != 0 {
        return Err(TaskStoreError::CorruptRecord(
            "partial planned-effect TaskWriteSet schema",
        ));
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(SCHEMA_V17_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v17 → v18 adds immutable owner endpoint proofs for planned effect slots.
/// Existing rows retain a zero endpoint root and no invented endpoint facts.
pub(crate) fn migrate_v18(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let table_present: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM sqlite_master
            WHERE type='table' AND name='task_write_set_effect_endpoints'
         )",
        [],
        |row| row.get(0),
    )?;
    let trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name IN
            ('task_write_set_effect_endpoint_is_immutable',
             'task_write_set_effect_endpoint_is_immutable_delete')",
        [],
        |row| row.get(0),
    )?;
    let root_column_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('task_write_sets')
         WHERE name = 'effect_endpoint_set_root'",
        [],
        |row| row.get(0),
    )?;
    if table_present && trigger_count == 2 && root_column_count == 1 {
        connection.pragma_update(None, "user_version", 18)?;
        return Ok(());
    }
    if table_present || trigger_count != 0 || root_column_count != 0 {
        return Err(TaskStoreError::CorruptRecord(
            "partial effect-endpoint TaskWriteSet schema",
        ));
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(SCHEMA_V18_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v18 → v19 adds the authority-checked proposed Artifact write declaration.
/// It is an intent root only; publication still requires a later Artifact
/// staging/publication receipt path.
pub(crate) fn migrate_v19(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let table_present: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM sqlite_master
            WHERE type='table' AND name='task_write_set_artifact_writes'
         )",
        [],
        |row| row.get(0),
    )?;
    let trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name IN
            ('task_write_set_artifact_write_is_immutable',
             'task_write_set_artifact_write_is_immutable_delete')",
        [],
        |row| row.get(0),
    )?;
    let root_column_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('task_write_sets')
         WHERE name = 'artifact_write_set_root'",
        [],
        |row| row.get(0),
    )?;
    if table_present && trigger_count == 2 && root_column_count == 1 {
        connection.pragma_update(None, "user_version", 19)?;
        return Ok(());
    }
    if table_present || trigger_count != 0 || root_column_count != 0 {
        return Err(TaskStoreError::CorruptRecord(
            "partial Artifact-write TaskWriteSet schema",
        ));
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(SCHEMA_V19_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v19 → v20 removes the historical equality check between the permit-bound
/// `write_set_root` and the canonical Artifact publication-plan root. A
/// sealed `TaskWriteSet` may now carry proposed Artifact writes whose staging
/// identity is chosen after permit issuance, so the two roots are durable but
/// distinct commitments.
pub(crate) fn migrate_v20(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let table_sql: String = connection.query_row(
        "SELECT COALESCE(
            (SELECT sql FROM sqlite_master
             WHERE type='table' AND name='task_artifact_commit_plans'), '')",
        [],
        |row| row.get(0),
    )?;
    let trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name IN
            ('task_artifact_commit_plan_identity_immutable',
             'task_artifact_commit_plan_no_delete')",
        [],
        |row| row.get(0),
    )?;
    let column_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('task_artifact_commit_plans')",
        [],
        |row| row.get(0),
    )?;
    if table_sql.is_empty() || trigger_count != 2 || column_count != 13 {
        return Err(TaskStoreError::CorruptRecord(
            "partial Artifact commit-plan schema",
        ));
    }
    let normalized_sql: String = table_sql
        .to_ascii_lowercase()
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    if !normalized_sql.contains("check(artifact_plan_root=write_set_root)") {
        connection.pragma_update(None, "user_version", 20)?;
        return Ok(());
    }

    connection.pragma_update(None, "foreign_keys", "OFF")?;
    let migration = (|| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(SCHEMA_V20_SQL)?;
        transaction.commit()?;
        Ok::<(), TaskStoreError>(())
    })();
    let restore = connection.pragma_update(None, "foreign_keys", "ON");
    if let Err(error) = migration {
        let _ = restore;
        return Err(error);
    }
    restore?;
    Ok(())
}

/// v20 → v21 adds owner-verified Semantic append declarations. The current
/// `SemanticAuthority` direct durable `AdmissionReceipt` is the only accepted
/// durability path in this slice; publication/finalization remains later.
pub(crate) fn migrate_v21(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let table_present: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM sqlite_master
            WHERE type='table' AND name='task_write_set_semantic_appends'
         )",
        [],
        |row| row.get(0),
    )?;
    let trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name IN
            ('task_write_set_semantic_append_is_immutable',
             'task_write_set_semantic_append_is_immutable_delete')",
        [],
        |row| row.get(0),
    )?;
    let root_column_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('task_write_sets')
         WHERE name = 'semantic_append_set_root'",
        [],
        |row| row.get(0),
    )?;
    if table_present && trigger_count == 2 && root_column_count == 1 {
        connection.pragma_update(None, "user_version", 21)?;
        return Ok(());
    }
    if table_present || trigger_count != 0 || root_column_count != 0 {
        return Err(TaskStoreError::CorruptRecord(
            "partial Semantic-append TaskWriteSet schema",
        ));
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(SCHEMA_V21_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v21 → v22 adds an optional owner-verified Semantic durability receipt ID.
/// Historical direct-Durable admission rows remain valid without this
/// secondary receipt and retain their v1 append-root formula.
pub(crate) fn migrate_v22(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let table_present: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM sqlite_master
            WHERE type='table' AND name='task_write_set_semantic_appends'
         )",
        [],
        |row| row.get(0),
    )?;
    if !table_present {
        return Err(TaskStoreError::CorruptRecord(
            "missing Semantic-append table before v22",
        ));
    }
    let column_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('task_write_set_semantic_appends')
         WHERE name = 'durability_receipt_id'",
        [],
        |row| row.get(0),
    )?;
    if column_count == 1 {
        connection.pragma_update(None, "user_version", 22)?;
        return Ok(());
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(SCHEMA_V22_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v22 → v23 adds the caller-declared Semantic admission-policy digest.
/// Historical rows retain `NULL` rather than receiving an invented policy
/// fact; new seals persist the owner-verified digest and include it in the
/// v3 append-set root.
pub(crate) fn migrate_v23(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let table_present: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM sqlite_master
            WHERE type='table' AND name='task_write_set_semantic_appends'
         )",
        [],
        |row| row.get(0),
    )?;
    if !table_present {
        return Err(TaskStoreError::CorruptRecord(
            "missing Semantic-append table before v23",
        ));
    }
    let column_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('task_write_set_semantic_appends')
         WHERE name = 'admission_policy_digest'",
        [],
        |row| row.get(0),
    )?;
    if column_count == 1 {
        connection.pragma_update(None, "user_version", 23)?;
        return Ok(());
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(SCHEMA_V23_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v23 → v24 widens the immutable participant and planned-effect endpoint
/// checks for the owner-verified Operation endpoint. `SQLite` cannot alter a
/// CHECK constraint in place, so both tables are copied in one transaction;
/// historical rows and their trigger guards remain byte-for-byte equivalent.
pub(crate) fn migrate_v24(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let participant_sql: Option<String> = connection
        .query_row(
            "SELECT sql FROM sqlite_master
             WHERE type='table' AND name='task_participants'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let endpoint_sql: Option<String> = connection
        .query_row(
            "SELECT sql FROM sqlite_master
             WHERE type='table' AND name='task_write_set_effect_endpoints'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name IN
            ('task_participants_immutable_update',
             'task_participants_immutable_delete',
             'task_write_set_effect_endpoint_is_immutable',
             'task_write_set_effect_endpoint_is_immutable_delete')",
        [],
        |row| row.get(0),
    )?;
    let normalize = |sql: &str| {
        sql.to_ascii_lowercase()
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>()
    };
    let participant_sql =
        participant_sql
            .as_deref()
            .map(normalize)
            .ok_or(TaskStoreError::CorruptRecord(
                "missing participant table before v24",
            ))?;
    let endpoint_sql =
        endpoint_sql
            .as_deref()
            .map(normalize)
            .ok_or(TaskStoreError::CorruptRecord(
                "missing effect-endpoint table before v24",
            ))?;
    let participant_wide = participant_sql.contains("check(participant_typebetween1and8)");
    let endpoint_wide = endpoint_sql.contains("check(endpoint_kindbetween1and6)");
    if participant_wide && endpoint_wide && trigger_count == 4 {
        connection.pragma_update(None, "user_version", 24)?;
        return Ok(());
    }
    let participant_old = participant_sql.contains("check(participant_typebetween1and7)");
    let endpoint_old = endpoint_sql.contains("check(endpoint_kindbetween1and5)");
    if !participant_old || !endpoint_old || trigger_count != 4 {
        return Err(TaskStoreError::CorruptRecord(
            "partial Operation endpoint schema migration",
        ));
    }

    connection.pragma_update(None, "foreign_keys", "OFF")?;
    let migration = (|| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(SCHEMA_V24_SQL)?;
        transaction.commit()?;
        Ok::<(), TaskStoreError>(())
    })();
    let restore = connection.pragma_update(None, "foreign_keys", "ON");
    if let Err(error) = migration {
        let _ = restore;
        return Err(error);
    }
    restore?;
    Ok(())
}

/// v24 → v25 adds the immutable Task-side Semantic publication plan and
/// nested owner receipt rows. Existing permits and Task receipts remain
/// unchanged; no publication fact is inferred during migration.
pub(crate) fn migrate_v25(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let table_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN
            ('task_semantic_commit_plans', 'task_semantic_publication_receipts')",
        [],
        |row| row.get(0),
    )?;
    let trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name IN
            ('task_semantic_commit_plan_identity_immutable',
             'task_semantic_commit_plan_no_delete',
             'task_semantic_publication_receipt_immutable_update',
             'task_semantic_publication_receipt_immutable_delete')",
        [],
        |row| row.get(0),
    )?;
    if table_count == 2 && trigger_count == 4 {
        connection.pragma_update(None, "user_version", 25)?;
        return Ok(());
    }
    if table_count != 0 || trigger_count != 0 {
        return Err(TaskStoreError::CorruptRecord(
            "partial Semantic publication schema",
        ));
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(SCHEMA_V25_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v25 → v26 adds the immutable typed finalize envelope used to recover a
/// mixed Effect + Semantic v3 request without caller memory.
pub(crate) fn migrate_v26(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let table_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN
            ('task_semantic_finalize_envelopes',
             'task_semantic_finalize_satisfactions')",
        [],
        |row| row.get(0),
    )?;
    let trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name IN
            ('task_semantic_finalize_envelope_immutable_update',
             'task_semantic_finalize_envelope_no_delete',
             'task_semantic_finalize_satisfaction_immutable_update',
             'task_semantic_finalize_satisfaction_immutable_delete')",
        [],
        |row| row.get(0),
    )?;
    if table_count == 2 && trigger_count == 4 {
        connection.pragma_update(None, "user_version", 26)?;
        return Ok(());
    }
    if table_count != 0 || trigger_count != 0 {
        return Err(TaskStoreError::CorruptRecord(
            "partial mixed finalize envelope schema",
        ));
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(SCHEMA_V26_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v26 → v27 adds the durable `TaskAuthority` lease/term and immutable
/// transition history used to fence stale cross-process holders.
pub(crate) fn migrate_v27(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let table_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN
            ('task_authority_leases', 'task_authority_lease_history')",
        [],
        |row| row.get(0),
    )?;
    let trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name IN
            ('task_authority_lease_history_immutable_update',
             'task_authority_lease_history_immutable_delete')",
        [],
        |row| row.get(0),
    )?;
    if table_count == 2 && trigger_count == 2 {
        connection.pragma_update(None, "user_version", 27)?;
        return Ok(());
    }
    if table_count != 0 || trigger_count != 0 {
        return Err(TaskStoreError::CorruptRecord(
            "partial authority lease schema",
        ));
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(SCHEMA_V27_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v27 → v28 adds the optional immutable lease binding copied into each
/// opt-in `CommitPermit`; legacy permits remain explicitly unbound.
pub(crate) fn migrate_v28(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let mut present = 0usize;
    let expected = [
        "authority_lease_authority_id",
        "authority_lease_holder_id",
        "authority_lease_term",
        "authority_lease_epoch",
        "authority_lease_fencing_token",
        "authority_lease_expires_at_ms",
    ];
    {
        let mut statement = connection.prepare("PRAGMA table_info(commit_permits)")?;
        let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
        for column in columns {
            if expected.contains(&column?.as_str()) {
                present += 1;
            }
        }
    }
    let trigger_present: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM sqlite_master
            WHERE type='trigger' AND name='commit_permit_authority_lease_binding_immutable'
         )",
        [],
        |row| row.get(0),
    )?;
    if present == expected.len() && trigger_present {
        connection.pragma_update(None, "user_version", 28)?;
        return Ok(());
    }
    if present != 0 || trigger_present {
        return Err(TaskStoreError::CorruptRecord(
            "partial authority lease permit binding schema",
        ));
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(SCHEMA_V28_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v28 → v29 adds the optional immutable lease binding copied into each
/// lease-aware `PermitAdoptionReceipt`; legacy adoption rows remain
/// explicitly unbound.
pub(crate) fn migrate_v29(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let mut present = 0usize;
    let expected = [
        "authority_lease_authority_id",
        "authority_lease_holder_id",
        "authority_lease_term",
        "authority_lease_epoch",
        "authority_lease_fencing_token",
        "authority_lease_expires_at_ms",
    ];
    {
        let mut statement = connection.prepare("PRAGMA table_info(task_adoption_receipts)")?;
        let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
        for column in columns {
            if expected.contains(&column?.as_str()) {
                present += 1;
            }
        }
    }
    let trigger_present: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM sqlite_master
            WHERE type='trigger' AND name='task_adoption_authority_lease_binding_immutable'
         )",
        [],
        |row| row.get(0),
    )?;
    if present == expected.len() && trigger_present {
        connection.pragma_update(None, "user_version", 29)?;
        return Ok(());
    }
    if present != 0 || trigger_present {
        return Err(TaskStoreError::CorruptRecord(
            "partial authority lease adoption binding schema",
        ));
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(SCHEMA_V29_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v29 → v30 adds the immutable local receipt for the
/// `FROZEN_FOR_TAKEOVER` lease fence. The exact fence-set union and barrier
/// receipts remain nullable until the distributed takeover path exists.
pub(crate) fn migrate_v30(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let table_present: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM sqlite_master
            WHERE type='table' AND name='task_authority_takeover_fence_receipts'
         )",
        [],
        |row| row.get(0),
    )?;
    let trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type='trigger' AND name IN (
             'task_authority_takeover_fence_receipt_immutable',
             'task_authority_takeover_fence_receipt_no_delete'
         )",
        [],
        |row| row.get(0),
    )?;
    if table_present && trigger_count == 2 {
        connection.pragma_update(None, "user_version", 30)?;
        return Ok(());
    }
    if table_present || trigger_count != 0 {
        return Err(TaskStoreError::CorruptRecord(
            "partial takeover fence receipt schema",
        ));
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(SCHEMA_V30_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v30 → v31 adds the immutable local assignment baseline used by
/// lease-bound permit paths. It does not create a successor assignment or a
/// takeover receipt; those remain a later barrier gate.
pub(crate) fn migrate_v31(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let table_present: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM sqlite_master
            WHERE type='table' AND name='task_authority_assignments'
         )",
        [],
        |row| row.get(0),
    )?;
    let trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type='trigger' AND name IN (
             'task_authority_assignment_identity_immutable',
             'task_authority_assignment_no_delete'
         )",
        [],
        |row| row.get(0),
    )?;
    if table_present && trigger_count == 2 {
        connection.pragma_update(None, "user_version", 31)?;
        return Ok(());
    }
    if table_present || trigger_count != 0 {
        return Err(TaskStoreError::CorruptRecord(
            "partial authority assignment schema",
        ));
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(SCHEMA_V31_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v31 → v32 adds the immutable pending prefix of a local
/// `TaskAuthorityTakeoverReceipt`. It links the old assignment to the local
/// fence receipt but cannot carry a successor assignment or remote barrier
/// completion yet.
pub(crate) fn migrate_v32(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let table_present: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM sqlite_master
            WHERE type='table' AND name='task_authority_takeover_receipts'
         )",
        [],
        |row| row.get(0),
    )?;
    let trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type='trigger' AND name IN (
             'task_authority_takeover_receipt_immutable',
             'task_authority_takeover_receipt_no_delete'
         )",
        [],
        |row| row.get(0),
    )?;
    if table_present && trigger_count == 2 {
        connection.pragma_update(None, "user_version", 32)?;
        return Ok(());
    }
    if table_present || trigger_count != 0 {
        return Err(TaskStoreError::CorruptRecord(
            "partial takeover receipt schema",
        ));
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(SCHEMA_V32_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v32 → v33 adds immutable per-endpoint takeover-barrier observations. The
/// rows bind to a pending takeover receipt and exact local fence-set root but
/// never advance the parent receipt or activate a successor assignment.
pub(crate) fn migrate_v33(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let table_present: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM sqlite_master
            WHERE type='table' AND name='task_authority_takeover_barrier_receipts'
         )",
        [],
        |row| row.get(0),
    )?;
    let trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type='trigger' AND name IN (
             'task_authority_takeover_barrier_receipt_immutable',
             'task_authority_takeover_barrier_receipt_no_delete'
         )",
        [],
        |row| row.get(0),
    )?;
    if table_present && trigger_count == 2 {
        connection.pragma_update(None, "user_version", 33)?;
        return Ok(());
    }
    if table_present || trigger_count != 0 {
        return Err(TaskStoreError::CorruptRecord(
            "partial takeover barrier receipt schema",
        ));
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(SCHEMA_V33_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v33 → v34 adds the canonical exact-fence member manifest used to match
/// endpoint barrier observations against the full locally provable set.
pub(crate) fn migrate_v34(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let table_present: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM sqlite_master
            WHERE type='table' AND name='task_authority_takeover_fence_members'
         )",
        [],
        |row| row.get(0),
    )?;
    let trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type='trigger' AND name IN (
             'task_authority_takeover_fence_member_immutable',
             'task_authority_takeover_fence_member_no_delete'
         )",
        [],
        |row| row.get(0),
    )?;
    if table_present && trigger_count == 2 {
        connection.pragma_update(None, "user_version", 34)?;
        return Ok(());
    }
    if table_present || trigger_count != 0 {
        return Err(TaskStoreError::CorruptRecord(
            "partial takeover fence member schema",
        ));
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(SCHEMA_V34_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v34 → v35 persists the endpoint-supplied barrier digest. Pre-v35 rows
/// retain `NULL` because their digest was never durably stored and cannot be
/// reconstructed from the legacy receipt identity.
pub(crate) fn migrate_v35(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let table_present: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM sqlite_master
            WHERE type='table' AND name='task_authority_takeover_barrier_receipts'
         )",
        [],
        |row| row.get(0),
    )?;
    let digest_column_present: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM pragma_table_info('task_authority_takeover_barrier_receipts')
            WHERE name='barrier_receipt_digest'
         )",
        [],
        |row| row.get(0),
    )?;
    let trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type='trigger' AND name IN (
             'task_authority_takeover_barrier_receipt_immutable',
             'task_authority_takeover_barrier_receipt_no_delete'
         )",
        [],
        |row| row.get(0),
    )?;
    if table_present && digest_column_present && trigger_count == 2 {
        connection.pragma_update(None, "user_version", 35)?;
        return Ok(());
    }
    if !table_present || trigger_count != 2 || digest_column_present {
        return Err(TaskStoreError::CorruptRecord(
            "partial takeover barrier digest schema",
        ));
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(SCHEMA_V35_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v35 → v36 adds the five optional NLOS principal signer columns plus the
/// coupled-presence trigger for barrier observations. Pre-v36 rows keep all
/// five columns `NULL`; they stay readable as legacy unsigned observations.
pub(crate) fn migrate_v36(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let table_present: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM sqlite_master
            WHERE type='table' AND name='task_authority_takeover_barrier_receipts'
         )",
        [],
        |row| row.get(0),
    )?;
    let signer_column_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('task_authority_takeover_barrier_receipts')
         WHERE name IN (
             'signer_principal_id', 'signer_control_domain_id', 'signer_key_id',
             'signer_key_generation', 'signer_signature'
         )",
        [],
        |row| row.get(0),
    )?;
    let trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type='trigger' AND name IN (
             'task_authority_takeover_barrier_receipt_immutable',
             'task_authority_takeover_barrier_receipt_no_delete'
         )",
        [],
        |row| row.get(0),
    )?;
    let coupled_trigger_present: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM sqlite_master
            WHERE type='trigger'
              AND name='task_authority_takeover_barrier_receipts_signer_coupled'
         )",
        [],
        |row| row.get(0),
    )?;
    if table_present && signer_column_count == 5 && trigger_count == 2 && coupled_trigger_present {
        connection.pragma_update(None, "user_version", 36)?;
        return Ok(());
    }
    if !table_present || trigger_count != 2 || coupled_trigger_present || signer_column_count != 0 {
        return Err(TaskStoreError::CorruptRecord(
            "partial v36 barrier signer schema",
        ));
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(SCHEMA_V36_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v36 → v37 relaxes the takeover receipt's `new_assignment_id` and
/// `barrier_state` CHECK constraints and replaces the blanket immutable
/// trigger with a narrowed guard that permits exactly one transition:
/// `Pending (1) → Complete (2)` while filling the previously-NULL successor
/// assignment identity, with every other column byte-equal. This unlocks
/// what the v32-v36 chain deliberately deferred: takeover completion and
/// successor assignment activation.
///
/// Unlike the v24 precedent this table is an FK parent
/// (`task_authority_takeover_barrier_receipts.takeover_receipt_id`), and
/// `SqliteTaskAuthority::open_with_vfs` enables `foreign_keys` before the
/// migration chain runs. `SQLite`'s `DROP TABLE` performs an implicit delete
/// that would trip immediate FK checks on durable child observations, so
/// the migration disables `foreign_keys` around the single
/// `BEGIN IMMEDIATE` copy transaction (the pragma is a silent no-op inside
/// a transaction) and restores the enforced state afterwards on every
/// path. Child foreign keys still resolve after the rename because they
/// name the final table name, which the copy restores.
pub(crate) fn migrate_v37(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let table_present: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM sqlite_master
            WHERE type='table' AND name='task_authority_takeover_receipts'
         )",
        [],
        |row| row.get(0),
    )?;
    let immutable_trigger_sql: Option<String> = connection
        .query_row(
            "SELECT sql FROM sqlite_master
             WHERE type='trigger' AND name='task_authority_takeover_receipt_immutable'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let no_delete_present: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM sqlite_master
            WHERE type='trigger' AND name='task_authority_takeover_receipt_no_delete'
         )",
        [],
        |row| row.get(0),
    )?;
    // Table-level CHECK constraints cannot be read back directly, so shape
    // detection uses the immutable trigger's stored SQL text: the v37 guard
    // mentions `OLD.barrier_state`/`NEW.barrier_state`, the v36 blanket
    // trigger mentions no barrier_state at all.
    let narrowed = immutable_trigger_sql
        .as_ref()
        .is_some_and(|sql| sql.contains("OLD.barrier_state") && sql.contains("NEW.barrier_state"));
    if table_present && narrowed && no_delete_present {
        connection.pragma_update(None, "user_version", 37)?;
        return Ok(());
    }
    if !table_present || immutable_trigger_sql.is_none() || !no_delete_present {
        return Err(TaskStoreError::CorruptRecord(
            "partial v37 takeover completion schema",
        ));
    }
    connection.pragma_update(None, "foreign_keys", "OFF")?;
    let migration = (|| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(SCHEMA_V37_SQL)?;
        transaction.commit()?;
        Ok::<(), TaskStoreError>(())
    })();
    connection.pragma_update(None, "foreign_keys", "ON")?;
    migration
}

/// v37 → v38 adds a separate immutable receipt plane for cross-term permit
/// adoption. Keeping it separate preserves the byte shape and same-term
/// lease semantics of the legacy `task_adoption_receipts` rows while making
/// the takeover proof fields mandatory for the new path.
pub(crate) fn migrate_v38(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let table_present: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM sqlite_master
            WHERE type='table' AND name='task_cross_term_adoption_receipts'
         )",
        [],
        |row| row.get(0),
    )?;
    let trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type='trigger' AND name IN (
             'task_cross_term_adoption_receipt_immutable',
             'task_cross_term_adoption_receipt_no_delete'
         )",
        [],
        |row| row.get(0),
    )?;
    if table_present && trigger_count == 2 {
        connection.pragma_update(None, "user_version", 38)?;
        return Ok(());
    }
    if table_present || trigger_count != 0 {
        return Err(TaskStoreError::CorruptRecord(
            "partial cross-term adoption receipt schema",
        ));
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(SCHEMA_V38_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v38 → v39 adds the immutable nested Resource cost receipt tables (one
/// parent row per sealed Reservation plus one child row per ordered
/// consumption) under a terminal Task receipt. The migration is additive
/// with no backfill: existing Task receipts remain valid and receive no
/// invented Resource evidence; a legacy receipt simply has no nested rows.
pub(crate) fn migrate_v39(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let complete_schema_parts: i64 = connection.query_row(
        "SELECT (SELECT COUNT(*) FROM sqlite_master
                 WHERE type='table' AND name IN (
                     'task_resource_cost_receipts',
                     'task_resource_cost_consumptions'))
              + (SELECT COUNT(*) FROM sqlite_master
                 WHERE type='trigger' AND name IN (
                     'task_resource_cost_receipt_immutable',
                     'task_resource_cost_receipt_no_delete',
                     'task_resource_cost_consumption_immutable',
                     'task_resource_cost_consumption_no_delete'))",
        [],
        |row| row.get(0),
    )?;
    if complete_schema_parts == 6 {
        connection.pragma_update(None, "user_version", 39)?;
        return Ok(());
    }
    if complete_schema_parts != 0 {
        return Err(TaskStoreError::CorruptRecord(
            "partial nested Resource cost receipt schema",
        ));
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(SCHEMA_V39_SQL)?;
    transaction.commit()?;
    Ok(())
}

const SCHEMA_V13_SQL: &str = "CREATE TABLE task_write_sets (
        task_id BLOB NOT NULL CHECK(length(task_id) = 16),
        attempt_id BLOB NOT NULL CHECK(length(attempt_id) = 16),
        attempt_generation BLOB NOT NULL CHECK(length(attempt_generation) = 8),
        idempotency_key BLOB NOT NULL CHECK(length(idempotency_key) = 16),
        snapshot_id BLOB NOT NULL CHECK(length(snapshot_id) = 16),
        snapshot_receipt_id BLOB NOT NULL CHECK(length(snapshot_receipt_id) = 16),
        expected_head_commit_seq BLOB NOT NULL CHECK(length(expected_head_commit_seq) = 8),
        effect_history_root BLOB NOT NULL CHECK(length(effect_history_root) = 32),
        retry_fence_epoch BLOB NOT NULL CHECK(length(retry_fence_epoch) = 8),
        group_id BLOB CHECK(group_id IS NULL OR length(group_id) = 16),
        membership_generation BLOB CHECK(membership_generation IS NULL OR length(membership_generation) = 8),
        membership_root BLOB CHECK(membership_root IS NULL OR length(membership_root) = 32),
        group_policy_digest BLOB CHECK(group_policy_digest IS NULL OR length(group_policy_digest) = 32),
        participant_registry_generation BLOB NOT NULL CHECK(length(participant_registry_generation) = 8),
        participant_registry_root BLOB NOT NULL CHECK(length(participant_registry_root) = 32),
        artifact_read_set_root BLOB NOT NULL CHECK(length(artifact_read_set_root) = 32),
        write_set_root BLOB NOT NULL CHECK(length(write_set_root) = 32),
        sealed_at_ms INTEGER NOT NULL CHECK(sealed_at_ms >= 0),
        PRIMARY KEY(task_id, idempotency_key),
        UNIQUE(task_id, write_set_root),
        FOREIGN KEY(task_id) REFERENCES tasks(task_id)
    ) STRICT;
    CREATE TABLE task_write_set_artifact_reads (
        task_id BLOB NOT NULL CHECK(length(task_id) = 16),
        idempotency_key BLOB NOT NULL CHECK(length(idempotency_key) = 16),
        read_seq INTEGER NOT NULL CHECK(read_seq >= 0),
        artifact_id BLOB NOT NULL CHECK(length(artifact_id) = 16),
        expected_head_revision BLOB NOT NULL CHECK(length(expected_head_revision) = 8),
        expected_head_digest BLOB CHECK(expected_head_digest IS NULL OR length(expected_head_digest) = 32),
        PRIMARY KEY(task_id, idempotency_key, read_seq),
        UNIQUE(task_id, idempotency_key, artifact_id),
        FOREIGN KEY(task_id, idempotency_key)
            REFERENCES task_write_sets(task_id, idempotency_key)
    ) STRICT;
    CREATE TRIGGER task_write_set_is_immutable
    BEFORE UPDATE ON task_write_sets
    BEGIN SELECT RAISE(ABORT, 'TaskWriteSet is immutable'); END;
    CREATE TRIGGER task_write_set_is_immutable_delete
    BEFORE DELETE ON task_write_sets
    BEGIN SELECT RAISE(ABORT, 'TaskWriteSet is immutable'); END;
    CREATE TRIGGER task_write_set_artifact_read_is_immutable
    BEFORE UPDATE ON task_write_set_artifact_reads
    BEGIN SELECT RAISE(ABORT, 'TaskWriteSet artifact read is immutable'); END;
    CREATE TRIGGER task_write_set_artifact_read_is_immutable_delete
    BEFORE DELETE ON task_write_set_artifact_reads
    BEGIN SELECT RAISE(ABORT, 'TaskWriteSet artifact read is immutable'); END;
    PRAGMA user_version = 13;";

const SCHEMA_V14_SQL: &str = "CREATE TABLE task_write_set_process_bindings (
        task_id BLOB NOT NULL CHECK(length(task_id) = 16),
        idempotency_key BLOB NOT NULL CHECK(length(idempotency_key) = 16),
        process_id BLOB NOT NULL CHECK(length(process_id) = 16),
        process_generation BLOB NOT NULL CHECK(length(process_generation) = 8),
        process_fencing_token BLOB NOT NULL CHECK(length(process_fencing_token) = 32),
        agent_instance_id BLOB NOT NULL CHECK(length(agent_instance_id) = 16),
        agent_instance_generation BLOB NOT NULL CHECK(length(agent_instance_generation) = 8),
        isolation_domain_id BLOB NOT NULL CHECK(length(isolation_domain_id) = 16),
        isolation_domain_generation BLOB NOT NULL CHECK(length(isolation_domain_generation) = 8),
        isolation_domain_fencing_token BLOB NOT NULL CHECK(length(isolation_domain_fencing_token) = 32),
        participant_id BLOB NOT NULL CHECK(length(participant_id) = 16),
        participant_generation BLOB NOT NULL CHECK(length(participant_generation) = 8),
        admission_receipt_id BLOB NOT NULL CHECK(length(admission_receipt_id) = 16),
        PRIMARY KEY(task_id, idempotency_key),
        FOREIGN KEY(task_id, idempotency_key)
            REFERENCES task_write_sets(task_id, idempotency_key)
    ) STRICT;
    CREATE TRIGGER task_write_set_process_binding_is_immutable
    BEFORE UPDATE ON task_write_set_process_bindings
    BEGIN SELECT RAISE(ABORT, 'TaskWriteSet Process binding is immutable'); END;
    CREATE TRIGGER task_write_set_process_binding_is_immutable_delete
    BEFORE DELETE ON task_write_set_process_bindings
    BEGIN SELECT RAISE(ABORT, 'TaskWriteSet Process binding is immutable'); END;
    PRAGMA user_version = 14;";

const SCHEMA_V15_SQL: &str = "DROP TRIGGER task_participants_immutable_update;
    DROP TRIGGER task_participants_immutable_delete;
    CREATE TABLE task_participants_v15 (
        registry_id BLOB NOT NULL CHECK(length(registry_id) = 16),
        participant_seq INTEGER NOT NULL CHECK(participant_seq >= 0),
        participant_type INTEGER NOT NULL CHECK(participant_type BETWEEN 1 AND 7),
        participant_id BLOB NOT NULL CHECK(length(participant_id) = 16),
        participant_generation BLOB NOT NULL CHECK(length(participant_generation) = 8),
        admission_receipt_id BLOB NOT NULL CHECK(length(admission_receipt_id) = 16),
        PRIMARY KEY(registry_id, participant_seq),
        UNIQUE(registry_id, participant_type, participant_id),
        FOREIGN KEY(registry_id) REFERENCES task_participant_registries(registry_id)
    ) STRICT;
    INSERT INTO task_participants_v15
        (registry_id, participant_seq, participant_type, participant_id,
         participant_generation, admission_receipt_id)
        SELECT registry_id, participant_seq, participant_type, participant_id,
               participant_generation, admission_receipt_id
        FROM task_participants;
    DROP TABLE task_participants;
    ALTER TABLE task_participants_v15 RENAME TO task_participants;
    CREATE TRIGGER task_participants_immutable_update BEFORE UPDATE ON task_participants
    BEGIN SELECT RAISE(ABORT, 'task participant is immutable'); END;
    CREATE TRIGGER task_participants_immutable_delete BEFORE DELETE ON task_participants
    BEGIN SELECT RAISE(ABORT, 'task participant is immutable'); END;
    PRAGMA user_version = 15;";

const SCHEMA_V16_SQL: &str = "ALTER TABLE task_write_sets
        ADD COLUMN semantic_read_set_root BLOB NOT NULL DEFAULT X'0000000000000000000000000000000000000000000000000000000000000000'
        CHECK(length(semantic_read_set_root) = 32);
    ALTER TABLE task_write_sets
        ADD COLUMN resource_reservation_set_root BLOB NOT NULL DEFAULT X'0000000000000000000000000000000000000000000000000000000000000000'
        CHECK(length(resource_reservation_set_root) = 32);
    CREATE TABLE task_write_set_semantic_reads (
        task_id BLOB NOT NULL CHECK(length(task_id) = 16),
        idempotency_key BLOB NOT NULL CHECK(length(idempotency_key) = 16),
        read_seq INTEGER NOT NULL CHECK(read_seq >= 0),
        event_id BLOB NOT NULL CHECK(length(event_id) = 32),
        expected_log_seq BLOB NOT NULL CHECK(length(expected_log_seq) = 8),
        expected_canonical_digest BLOB NOT NULL CHECK(length(expected_canonical_digest) = 32),
        PRIMARY KEY(task_id, idempotency_key, read_seq),
        UNIQUE(task_id, idempotency_key, event_id),
        FOREIGN KEY(task_id, idempotency_key)
            REFERENCES task_write_sets(task_id, idempotency_key)
    ) STRICT;
    CREATE TABLE task_write_set_resource_reservations (
        task_id BLOB NOT NULL CHECK(length(task_id) = 16),
        idempotency_key BLOB NOT NULL CHECK(length(idempotency_key) = 16),
        read_seq INTEGER NOT NULL CHECK(read_seq >= 0),
        reservation_id BLOB NOT NULL CHECK(length(reservation_id) = 16),
        account_id BLOB NOT NULL CHECK(length(account_id) = 16),
        quote_id BLOB NOT NULL CHECK(length(quote_id) = 16),
        call_id BLOB NOT NULL CHECK(length(call_id) = 16),
        operation_id BLOB NOT NULL CHECK(length(operation_id) = 16),
        driver_id BLOB NOT NULL CHECK(length(driver_id) = 16),
        device_id BLOB NOT NULL CHECK(length(device_id) = 16),
        driver_generation BLOB NOT NULL CHECK(length(driver_generation) = 8),
        driver_fencing_token BLOB NOT NULL CHECK(length(driver_fencing_token) = 32),
        upper_bound BLOB NOT NULL CHECK(length(upper_bound) = 8),
        PRIMARY KEY(task_id, idempotency_key, read_seq),
        UNIQUE(task_id, idempotency_key, reservation_id),
        FOREIGN KEY(task_id, idempotency_key)
            REFERENCES task_write_sets(task_id, idempotency_key)
    ) STRICT;
    CREATE TRIGGER task_write_set_semantic_read_is_immutable
    BEFORE UPDATE ON task_write_set_semantic_reads
    BEGIN SELECT RAISE(ABORT, 'TaskWriteSet Semantic read is immutable'); END;
    CREATE TRIGGER task_write_set_semantic_read_is_immutable_delete
    BEFORE DELETE ON task_write_set_semantic_reads
    BEGIN SELECT RAISE(ABORT, 'TaskWriteSet Semantic read is immutable'); END;
    CREATE TRIGGER task_write_set_resource_reservation_is_immutable
    BEFORE UPDATE ON task_write_set_resource_reservations
    BEGIN SELECT RAISE(ABORT, 'TaskWriteSet Resource Reservation is immutable'); END;
    CREATE TRIGGER task_write_set_resource_reservation_is_immutable_delete
    BEFORE DELETE ON task_write_set_resource_reservations
    BEGIN SELECT RAISE(ABORT, 'TaskWriteSet Resource Reservation is immutable'); END;
    PRAGMA user_version = 16;";

const SCHEMA_V17_SQL: &str = "ALTER TABLE task_write_sets
        ADD COLUMN effect_set_root BLOB NOT NULL DEFAULT X'0000000000000000000000000000000000000000000000000000000000000000'
        CHECK(length(effect_set_root) = 32);
    CREATE TABLE task_write_set_planned_effects (
        task_id BLOB NOT NULL CHECK(length(task_id) = 16),
        idempotency_key BLOB NOT NULL CHECK(length(idempotency_key) = 16),
        effect_seq INTEGER NOT NULL CHECK(effect_seq >= 0),
        intent_spec_id BLOB NOT NULL CHECK(length(intent_spec_id) = 32),
        stable_action_slot BLOB NOT NULL CHECK(length(stable_action_slot) = 8),
        target_authority_object_id BLOB NOT NULL CHECK(length(target_authority_object_id) = 32),
        effect_class INTEGER NOT NULL CHECK(effect_class BETWEEN 0 AND 4294967295),
        idempotency_scope INTEGER NOT NULL CHECK(idempotency_scope BETWEEN 0 AND 4294967295),
        logical_effect_id BLOB NOT NULL CHECK(length(logical_effect_id) = 32),
        idempotency_identity_digest BLOB NOT NULL CHECK(length(idempotency_identity_digest) = 32),
        required INTEGER NOT NULL CHECK(required IN (0, 1)),
        required_condition_digest BLOB CHECK(required_condition_digest IS NULL OR length(required_condition_digest) = 32),
        success_criteria_digest BLOB NOT NULL CHECK(length(success_criteria_digest) = 32),
        action_proposal_digest BLOB NOT NULL CHECK(length(action_proposal_digest) = 32),
        PRIMARY KEY(task_id, idempotency_key, effect_seq),
        UNIQUE(task_id, idempotency_key, logical_effect_id),
        FOREIGN KEY(task_id, idempotency_key)
            REFERENCES task_write_sets(task_id, idempotency_key)
    ) STRICT;
    CREATE TRIGGER task_write_set_planned_effect_is_immutable
    BEFORE UPDATE ON task_write_set_planned_effects
    BEGIN SELECT RAISE(ABORT, 'TaskWriteSet planned effect is immutable'); END;
    CREATE TRIGGER task_write_set_planned_effect_is_immutable_delete
    BEFORE DELETE ON task_write_set_planned_effects
    BEGIN SELECT RAISE(ABORT, 'TaskWriteSet planned effect is immutable'); END;
    PRAGMA user_version = 17;";

const SCHEMA_V18_SQL: &str = "ALTER TABLE task_write_sets
        ADD COLUMN effect_endpoint_set_root BLOB NOT NULL DEFAULT X'0000000000000000000000000000000000000000000000000000000000000000'
        CHECK(length(effect_endpoint_set_root) = 32);
    CREATE TABLE task_write_set_effect_endpoints (
        task_id BLOB NOT NULL CHECK(length(task_id) = 16),
        idempotency_key BLOB NOT NULL CHECK(length(idempotency_key) = 16),
        endpoint_seq INTEGER NOT NULL CHECK(endpoint_seq >= 0),
        effect_seq INTEGER NOT NULL CHECK(effect_seq >= 0),
        endpoint_kind INTEGER NOT NULL CHECK(endpoint_kind BETWEEN 1 AND 5),
        object_id BLOB NOT NULL CHECK(length(object_id) = 16),
        participant_id BLOB NOT NULL CHECK(length(participant_id) = 16),
        participant_generation BLOB NOT NULL CHECK(length(participant_generation) = 8),
        admission_receipt_id BLOB NOT NULL CHECK(length(admission_receipt_id) = 16),
        PRIMARY KEY(task_id, idempotency_key, endpoint_seq),
        UNIQUE(task_id, idempotency_key, effect_seq, endpoint_kind, object_id),
        FOREIGN KEY(task_id, idempotency_key)
            REFERENCES task_write_sets(task_id, idempotency_key)
    ) STRICT;
    CREATE TRIGGER task_write_set_effect_endpoint_is_immutable
    BEFORE UPDATE ON task_write_set_effect_endpoints
    BEGIN SELECT RAISE(ABORT, 'TaskWriteSet effect endpoint is immutable'); END;
    CREATE TRIGGER task_write_set_effect_endpoint_is_immutable_delete
    BEFORE DELETE ON task_write_set_effect_endpoints
    BEGIN SELECT RAISE(ABORT, 'TaskWriteSet effect endpoint is immutable'); END;
    PRAGMA user_version = 18;";

const SCHEMA_V19_SQL: &str = "ALTER TABLE task_write_sets
        ADD COLUMN artifact_write_set_root BLOB NOT NULL DEFAULT X'0000000000000000000000000000000000000000000000000000000000000000'
        CHECK(length(artifact_write_set_root) = 32);
    CREATE TABLE task_write_set_artifact_writes (
        task_id BLOB NOT NULL CHECK(length(task_id) = 16),
        idempotency_key BLOB NOT NULL CHECK(length(idempotency_key) = 16),
        write_seq INTEGER NOT NULL CHECK(write_seq >= 0),
        artifact_id BLOB NOT NULL CHECK(length(artifact_id) = 16),
        expected_head_revision BLOB NOT NULL CHECK(length(expected_head_revision) = 8),
        proposed_revision BLOB NOT NULL CHECK(length(proposed_revision) = 8),
        content_digest BLOB NOT NULL CHECK(length(content_digest) = 32),
        size_bytes BLOB NOT NULL CHECK(length(size_bytes) = 8),
        PRIMARY KEY(task_id, idempotency_key, write_seq),
        UNIQUE(task_id, idempotency_key, artifact_id),
        FOREIGN KEY(task_id, idempotency_key)
            REFERENCES task_write_sets(task_id, idempotency_key)
    ) STRICT;
    CREATE TRIGGER task_write_set_artifact_write_is_immutable
    BEFORE UPDATE ON task_write_set_artifact_writes
    BEGIN SELECT RAISE(ABORT, 'TaskWriteSet Artifact write is immutable'); END;
    CREATE TRIGGER task_write_set_artifact_write_is_immutable_delete
    BEFORE DELETE ON task_write_set_artifact_writes
    BEGIN SELECT RAISE(ABORT, 'TaskWriteSet Artifact write is immutable'); END;
    PRAGMA user_version = 19;";

const SCHEMA_V20_SQL: &str = "DROP TRIGGER IF EXISTS task_artifact_commit_plan_identity_immutable;
    DROP TRIGGER IF EXISTS task_artifact_commit_plan_no_delete;
    CREATE TABLE task_artifact_commit_plans_v20 (
        plan_id BLOB PRIMARY KEY NOT NULL CHECK(length(plan_id) = 16),
        task_id BLOB NOT NULL CHECK(length(task_id) = 16),
        permit_id BLOB NOT NULL UNIQUE CHECK(length(permit_id) = 16),
        idempotency_key BLOB NOT NULL CHECK(length(idempotency_key) = 16),
        attempt_id BLOB NOT NULL CHECK(length(attempt_id) = 16),
        attempt_generation BLOB NOT NULL CHECK(length(attempt_generation) = 8),
        write_set_root BLOB NOT NULL CHECK(length(write_set_root) = 32),
        artifact_plan_root BLOB NOT NULL CHECK(length(artifact_plan_root) = 32),
        expected_artifact_count BLOB NOT NULL CHECK(length(expected_artifact_count) = 8),
        plan_state INTEGER NOT NULL CHECK(plan_state IN (0, 1, 2, 3)),
        task_receipt_id BLOB CHECK(task_receipt_id IS NULL OR length(task_receipt_id) = 16),
        created_at_ms INTEGER NOT NULL,
        updated_at_ms INTEGER NOT NULL,
        UNIQUE(task_id, idempotency_key),
        FOREIGN KEY(task_id) REFERENCES tasks(task_id),
        FOREIGN KEY(permit_id) REFERENCES commit_permits(permit_id),
        CHECK((plan_state = 3) = (task_receipt_id IS NOT NULL))
     ) STRICT;
    INSERT INTO task_artifact_commit_plans_v20 (
        plan_id, task_id, permit_id, idempotency_key, attempt_id,
        attempt_generation, write_set_root, artifact_plan_root,
        expected_artifact_count, plan_state, task_receipt_id,
        created_at_ms, updated_at_ms
    ) SELECT plan_id, task_id, permit_id, idempotency_key, attempt_id,
        attempt_generation, write_set_root, artifact_plan_root,
        expected_artifact_count, plan_state, task_receipt_id,
        created_at_ms, updated_at_ms
      FROM task_artifact_commit_plans;
    DROP TABLE task_artifact_commit_plans;
    ALTER TABLE task_artifact_commit_plans_v20 RENAME TO task_artifact_commit_plans;
    CREATE TRIGGER task_artifact_commit_plan_identity_immutable
    BEFORE UPDATE ON task_artifact_commit_plans
    WHEN OLD.plan_id IS NOT NEW.plan_id
      OR OLD.task_id IS NOT NEW.task_id
      OR OLD.permit_id IS NOT NEW.permit_id
      OR OLD.idempotency_key IS NOT NEW.idempotency_key
      OR OLD.attempt_id IS NOT NEW.attempt_id
      OR OLD.attempt_generation IS NOT NEW.attempt_generation
      OR OLD.write_set_root IS NOT NEW.write_set_root
      OR OLD.artifact_plan_root IS NOT NEW.artifact_plan_root
      OR OLD.expected_artifact_count IS NOT NEW.expected_artifact_count
      OR OLD.created_at_ms IS NOT NEW.created_at_ms
    BEGIN
        SELECT RAISE(ABORT, 'artifact commit plan identity is immutable');
    END;
    CREATE TRIGGER task_artifact_commit_plan_no_delete
    BEFORE DELETE ON task_artifact_commit_plans
    BEGIN
        SELECT RAISE(ABORT, 'artifact commit plan is durable evidence');
    END;
    PRAGMA user_version = 20;";

const SCHEMA_V21_SQL: &str = "ALTER TABLE task_write_sets
        ADD COLUMN semantic_append_set_root BLOB NOT NULL DEFAULT X'0000000000000000000000000000000000000000000000000000000000000000'
        CHECK(length(semantic_append_set_root) = 32);
    CREATE TABLE task_write_set_semantic_appends (
        task_id BLOB NOT NULL CHECK(length(task_id) = 16),
        idempotency_key BLOB NOT NULL CHECK(length(idempotency_key) = 16),
        append_seq INTEGER NOT NULL CHECK(append_seq >= 0),
        event_id BLOB NOT NULL CHECK(length(event_id) = 32),
        target_scope_kind INTEGER NOT NULL CHECK(target_scope_kind IN (1, 2)),
        target_scope_id BLOB NOT NULL CHECK(length(target_scope_id) = 16),
        required_durability INTEGER NOT NULL CHECK(required_durability = 2),
        admission_receipt_id BLOB NOT NULL CHECK(length(admission_receipt_id) = 16),
        PRIMARY KEY(task_id, idempotency_key, append_seq),
        UNIQUE(task_id, idempotency_key, event_id),
        FOREIGN KEY(task_id, idempotency_key)
            REFERENCES task_write_sets(task_id, idempotency_key)
    ) STRICT;
    CREATE TRIGGER task_write_set_semantic_append_is_immutable
    BEFORE UPDATE ON task_write_set_semantic_appends
    BEGIN SELECT RAISE(ABORT, 'TaskWriteSet Semantic append is immutable'); END;
    CREATE TRIGGER task_write_set_semantic_append_is_immutable_delete
    BEFORE DELETE ON task_write_set_semantic_appends
    BEGIN SELECT RAISE(ABORT, 'TaskWriteSet Semantic append is immutable'); END;
    PRAGMA user_version = 21;";

const SCHEMA_V22_SQL: &str = "ALTER TABLE task_write_set_semantic_appends
        ADD COLUMN durability_receipt_id BLOB
        CHECK(durability_receipt_id IS NULL OR length(durability_receipt_id) = 16);
    PRAGMA user_version = 22;";

const SCHEMA_V23_SQL: &str = "ALTER TABLE task_write_set_semantic_appends
        ADD COLUMN admission_policy_digest BLOB
        CHECK(admission_policy_digest IS NULL OR length(admission_policy_digest) = 32);
    PRAGMA user_version = 23;";

const SCHEMA_V24_SQL: &str = "DROP TRIGGER task_participants_immutable_update;
    DROP TRIGGER task_participants_immutable_delete;
    CREATE TABLE task_participants_v24 (
        registry_id BLOB NOT NULL CHECK(length(registry_id) = 16),
        participant_seq INTEGER NOT NULL CHECK(participant_seq >= 0),
        participant_type INTEGER NOT NULL CHECK(participant_type BETWEEN 1 AND 8),
        participant_id BLOB NOT NULL CHECK(length(participant_id) = 16),
        participant_generation BLOB NOT NULL CHECK(length(participant_generation) = 8),
        admission_receipt_id BLOB NOT NULL CHECK(length(admission_receipt_id) = 16),
        PRIMARY KEY(registry_id, participant_seq),
        UNIQUE(registry_id, participant_type, participant_id),
        FOREIGN KEY(registry_id) REFERENCES task_participant_registries(registry_id)
    ) STRICT;
    INSERT INTO task_participants_v24
        (registry_id, participant_seq, participant_type, participant_id,
         participant_generation, admission_receipt_id)
        SELECT registry_id, participant_seq, participant_type, participant_id,
               participant_generation, admission_receipt_id
        FROM task_participants;
    DROP TABLE task_participants;
    ALTER TABLE task_participants_v24 RENAME TO task_participants;
    CREATE TRIGGER task_participants_immutable_update BEFORE UPDATE ON task_participants
    BEGIN SELECT RAISE(ABORT, 'task participant is immutable'); END;
    CREATE TRIGGER task_participants_immutable_delete BEFORE DELETE ON task_participants
    BEGIN SELECT RAISE(ABORT, 'task participant is immutable'); END;
    DROP TRIGGER task_write_set_effect_endpoint_is_immutable;
    DROP TRIGGER task_write_set_effect_endpoint_is_immutable_delete;
    CREATE TABLE task_write_set_effect_endpoints_v24 (
        task_id BLOB NOT NULL CHECK(length(task_id) = 16),
        idempotency_key BLOB NOT NULL CHECK(length(idempotency_key) = 16),
        endpoint_seq INTEGER NOT NULL CHECK(endpoint_seq >= 0),
        effect_seq INTEGER NOT NULL CHECK(effect_seq >= 0),
        endpoint_kind INTEGER NOT NULL CHECK(endpoint_kind BETWEEN 1 AND 6),
        object_id BLOB NOT NULL CHECK(length(object_id) = 16),
        participant_id BLOB NOT NULL CHECK(length(participant_id) = 16),
        participant_generation BLOB NOT NULL CHECK(length(participant_generation) = 8),
        admission_receipt_id BLOB NOT NULL CHECK(length(admission_receipt_id) = 16),
        PRIMARY KEY(task_id, idempotency_key, endpoint_seq),
        UNIQUE(task_id, idempotency_key, effect_seq, endpoint_kind, object_id),
        FOREIGN KEY(task_id, idempotency_key)
            REFERENCES task_write_sets(task_id, idempotency_key)
    ) STRICT;
    INSERT INTO task_write_set_effect_endpoints_v24
        (task_id, idempotency_key, endpoint_seq, effect_seq, endpoint_kind,
         object_id, participant_id, participant_generation, admission_receipt_id)
        SELECT task_id, idempotency_key, endpoint_seq, effect_seq, endpoint_kind,
               object_id, participant_id, participant_generation, admission_receipt_id
        FROM task_write_set_effect_endpoints;
    DROP TABLE task_write_set_effect_endpoints;
    ALTER TABLE task_write_set_effect_endpoints_v24
        RENAME TO task_write_set_effect_endpoints;
    CREATE TRIGGER task_write_set_effect_endpoint_is_immutable
    BEFORE UPDATE ON task_write_set_effect_endpoints
    BEGIN SELECT RAISE(ABORT, 'TaskWriteSet effect endpoint is immutable'); END;
    CREATE TRIGGER task_write_set_effect_endpoint_is_immutable_delete
    BEFORE DELETE ON task_write_set_effect_endpoints
    BEGIN SELECT RAISE(ABORT, 'TaskWriteSet effect endpoint is immutable'); END;
    PRAGMA user_version = 24;";

pub(crate) const SCHEMA_V25_SQL: &str = "CREATE TABLE task_semantic_commit_plans (
        plan_id BLOB PRIMARY KEY NOT NULL CHECK(length(plan_id) = 16),
        task_id BLOB NOT NULL CHECK(length(task_id) = 16),
        permit_id BLOB NOT NULL UNIQUE CHECK(length(permit_id) = 16),
        idempotency_key BLOB NOT NULL CHECK(length(idempotency_key) = 16),
        attempt_id BLOB NOT NULL CHECK(length(attempt_id) = 16),
        attempt_generation BLOB NOT NULL CHECK(length(attempt_generation) = 8),
        write_set_root BLOB NOT NULL CHECK(length(write_set_root) = 32),
        semantic_append_set_root BLOB NOT NULL CHECK(length(semantic_append_set_root) = 32),
        expected_semantic_count BLOB NOT NULL CHECK(length(expected_semantic_count) = 8),
        plan_state INTEGER NOT NULL CHECK(plan_state IN (0, 1, 2, 3)),
        task_receipt_id BLOB CHECK(task_receipt_id IS NULL OR length(task_receipt_id) = 16),
        created_at_ms INTEGER NOT NULL,
        updated_at_ms INTEGER NOT NULL,
        UNIQUE(task_id, idempotency_key),
        FOREIGN KEY(task_id) REFERENCES tasks(task_id),
        FOREIGN KEY(permit_id) REFERENCES commit_permits(permit_id),
        CHECK((plan_state = 3) = (task_receipt_id IS NOT NULL))
     ) STRICT;

     CREATE TABLE task_semantic_publication_receipts (
        plan_id BLOB NOT NULL CHECK(length(plan_id) = 16),
        receipt_id BLOB PRIMARY KEY NOT NULL CHECK(length(receipt_id) = 16),
        task_id BLOB NOT NULL CHECK(length(task_id) = 16),
        permit_id BLOB NOT NULL CHECK(length(permit_id) = 16),
        write_set_root BLOB NOT NULL CHECK(length(write_set_root) = 32),
        event_id BLOB NOT NULL CHECK(length(event_id) = 32),
        target_scope_kind INTEGER NOT NULL CHECK(target_scope_kind IN (1, 2)),
        target_scope_id BLOB NOT NULL CHECK(length(target_scope_id) = 16),
        log_seq BLOB NOT NULL CHECK(length(log_seq) = 8),
        admission_receipt_id BLOB NOT NULL CHECK(length(admission_receipt_id) = 16),
        durability_receipt_id BLOB CHECK(durability_receipt_id IS NULL OR length(durability_receipt_id) = 16),
        semantic_checkpoint_after BLOB NOT NULL CHECK(length(semantic_checkpoint_after) = 32),
        created_at_ms INTEGER NOT NULL,
        UNIQUE(plan_id, event_id),
        UNIQUE(plan_id, receipt_id),
        FOREIGN KEY(plan_id) REFERENCES task_semantic_commit_plans(plan_id)
     ) STRICT;

     CREATE INDEX task_semantic_publication_receipts_by_task_permit
        ON task_semantic_publication_receipts(task_id, permit_id);

     CREATE TRIGGER task_semantic_commit_plan_identity_immutable
     BEFORE UPDATE ON task_semantic_commit_plans
     WHEN OLD.plan_id IS NOT NEW.plan_id
       OR OLD.task_id IS NOT NEW.task_id
       OR OLD.permit_id IS NOT NEW.permit_id
       OR OLD.idempotency_key IS NOT NEW.idempotency_key
       OR OLD.attempt_id IS NOT NEW.attempt_id
       OR OLD.attempt_generation IS NOT NEW.attempt_generation
       OR OLD.write_set_root IS NOT NEW.write_set_root
       OR OLD.semantic_append_set_root IS NOT NEW.semantic_append_set_root
       OR OLD.expected_semantic_count IS NOT NEW.expected_semantic_count
       OR OLD.created_at_ms IS NOT NEW.created_at_ms
     BEGIN
        SELECT RAISE(ABORT, 'semantic commit plan identity is immutable');
     END;

     CREATE TRIGGER task_semantic_commit_plan_no_delete
     BEFORE DELETE ON task_semantic_commit_plans
     BEGIN
        SELECT RAISE(ABORT, 'semantic commit plan is durable evidence');
     END;

     CREATE TRIGGER task_semantic_publication_receipt_immutable_update
     BEFORE UPDATE ON task_semantic_publication_receipts
     BEGIN
        SELECT RAISE(ABORT, 'nested semantic publication receipt is immutable');
     END;

     CREATE TRIGGER task_semantic_publication_receipt_immutable_delete
     BEFORE DELETE ON task_semantic_publication_receipts
     BEGIN
        SELECT RAISE(ABORT, 'nested semantic publication receipt is durable evidence');
     END;

     PRAGMA user_version = 25;";

pub(crate) const SCHEMA_V26_SQL: &str = "CREATE TABLE task_semantic_finalize_envelopes (
        plan_id BLOB PRIMARY KEY NOT NULL CHECK(length(plan_id) = 16),
        fenced_participant_digest BLOB NOT NULL CHECK(length(fenced_participant_digest) = 32),
        prepared_at_ms INTEGER NOT NULL CHECK(prepared_at_ms >= 0),
        FOREIGN KEY(plan_id) REFERENCES task_semantic_commit_plans(plan_id)
     ) STRICT;

     CREATE TABLE task_semantic_finalize_satisfactions (
        plan_id BLOB NOT NULL CHECK(length(plan_id) = 16),
        effect_seq BLOB NOT NULL CHECK(length(effect_seq) = 8),
        proof_kind INTEGER NOT NULL CHECK(proof_kind IN (0, 1)),
        proof_digest BLOB NOT NULL CHECK(length(proof_digest) = 32),
        PRIMARY KEY(plan_id, effect_seq),
        FOREIGN KEY(plan_id) REFERENCES task_semantic_finalize_envelopes(plan_id)
     ) STRICT;

     CREATE TRIGGER task_semantic_finalize_envelope_immutable_update
     BEFORE UPDATE ON task_semantic_finalize_envelopes
     BEGIN
        SELECT RAISE(ABORT, 'mixed finalize envelope is immutable');
     END;

     CREATE TRIGGER task_semantic_finalize_envelope_no_delete
     BEFORE DELETE ON task_semantic_finalize_envelopes
     BEGIN
        SELECT RAISE(ABORT, 'mixed finalize envelope is durable evidence');
     END;

     CREATE TRIGGER task_semantic_finalize_satisfaction_immutable_update
     BEFORE UPDATE ON task_semantic_finalize_satisfactions
     BEGIN
        SELECT RAISE(ABORT, 'mixed finalize satisfaction is immutable');
     END;

     CREATE TRIGGER task_semantic_finalize_satisfaction_immutable_delete
     BEFORE DELETE ON task_semantic_finalize_satisfactions
     BEGIN
        SELECT RAISE(ABORT, 'mixed finalize satisfaction is durable evidence');
     END;

     PRAGMA user_version = 26;";

pub(crate) const SCHEMA_V27_SQL: &str = "CREATE TABLE task_authority_leases (
        authority_id BLOB PRIMARY KEY NOT NULL CHECK(length(authority_id) = 16),
        holder_id BLOB NOT NULL CHECK(length(holder_id) = 16),
        term BLOB NOT NULL CHECK(length(term) = 8),
        lease_epoch BLOB NOT NULL CHECK(length(lease_epoch) = 8),
        fencing_token BLOB NOT NULL CHECK(length(fencing_token) = 32),
        requested_at_ms INTEGER NOT NULL CHECK(requested_at_ms >= 0),
        expires_at_ms INTEGER NOT NULL CHECK(expires_at_ms > requested_at_ms),
        ttl_ms INTEGER NOT NULL CHECK(ttl_ms > 0),
        idempotency_key BLOB NOT NULL CHECK(length(idempotency_key) = 16)
     ) STRICT;

     CREATE TABLE task_authority_lease_history (
        authority_id BLOB NOT NULL CHECK(length(authority_id) = 16),
        lease_epoch BLOB NOT NULL CHECK(length(lease_epoch) = 8),
        term BLOB NOT NULL CHECK(length(term) = 8),
        holder_id BLOB NOT NULL CHECK(length(holder_id) = 16),
        fencing_token BLOB NOT NULL CHECK(length(fencing_token) = 32),
        idempotency_key BLOB NOT NULL CHECK(length(idempotency_key) = 16),
        requested_at_ms INTEGER NOT NULL CHECK(requested_at_ms >= 0),
        expires_at_ms INTEGER NOT NULL CHECK(expires_at_ms > requested_at_ms),
        ttl_ms INTEGER NOT NULL CHECK(ttl_ms > 0),
        transition_kind INTEGER NOT NULL CHECK(transition_kind BETWEEN 1 AND 3),
        PRIMARY KEY(authority_id, lease_epoch),
        UNIQUE(authority_id, idempotency_key),
        FOREIGN KEY(authority_id) REFERENCES task_authority_leases(authority_id)
     ) STRICT;

     CREATE TRIGGER task_authority_lease_history_immutable_update
     BEFORE UPDATE ON task_authority_lease_history
     BEGIN
        SELECT RAISE(ABORT, 'authority lease history is immutable');
     END;

     CREATE TRIGGER task_authority_lease_history_immutable_delete
     BEFORE DELETE ON task_authority_lease_history
     BEGIN
        SELECT RAISE(ABORT, 'authority lease history is durable evidence');
     END;

     PRAGMA user_version = 27;";

pub(crate) const SCHEMA_V28_SQL: &str = "ALTER TABLE commit_permits
        ADD COLUMN authority_lease_authority_id BLOB
            CHECK(authority_lease_authority_id IS NULL OR length(authority_lease_authority_id) = 16);
     ALTER TABLE commit_permits
        ADD COLUMN authority_lease_holder_id BLOB
            CHECK(authority_lease_holder_id IS NULL OR length(authority_lease_holder_id) = 16);
     ALTER TABLE commit_permits
        ADD COLUMN authority_lease_term BLOB
            CHECK(authority_lease_term IS NULL OR length(authority_lease_term) = 8);
     ALTER TABLE commit_permits
        ADD COLUMN authority_lease_epoch BLOB
            CHECK(authority_lease_epoch IS NULL OR length(authority_lease_epoch) = 8);
     ALTER TABLE commit_permits
        ADD COLUMN authority_lease_fencing_token BLOB
            CHECK(authority_lease_fencing_token IS NULL OR length(authority_lease_fencing_token) = 32);
     ALTER TABLE commit_permits
        ADD COLUMN authority_lease_expires_at_ms INTEGER;

     CREATE TRIGGER commit_permit_authority_lease_binding_immutable
     BEFORE UPDATE ON commit_permits
     WHEN NEW.authority_lease_authority_id IS NOT OLD.authority_lease_authority_id
       OR NEW.authority_lease_holder_id IS NOT OLD.authority_lease_holder_id
       OR NEW.authority_lease_term IS NOT OLD.authority_lease_term
       OR NEW.authority_lease_epoch IS NOT OLD.authority_lease_epoch
       OR NEW.authority_lease_fencing_token IS NOT OLD.authority_lease_fencing_token
       OR NEW.authority_lease_expires_at_ms IS NOT OLD.authority_lease_expires_at_ms
     BEGIN
        SELECT RAISE(ABORT, 'authority lease permit binding is immutable');
     END;

     PRAGMA user_version = 28;";

pub(crate) const SCHEMA_V29_SQL: &str = "ALTER TABLE task_adoption_receipts
        ADD COLUMN authority_lease_authority_id BLOB
            CHECK(authority_lease_authority_id IS NULL OR length(authority_lease_authority_id) = 16);
     ALTER TABLE task_adoption_receipts
        ADD COLUMN authority_lease_holder_id BLOB
            CHECK(authority_lease_holder_id IS NULL OR length(authority_lease_holder_id) = 16);
     ALTER TABLE task_adoption_receipts
        ADD COLUMN authority_lease_term BLOB
            CHECK(authority_lease_term IS NULL OR length(authority_lease_term) = 8);
     ALTER TABLE task_adoption_receipts
        ADD COLUMN authority_lease_epoch BLOB
            CHECK(authority_lease_epoch IS NULL OR length(authority_lease_epoch) = 8);
     ALTER TABLE task_adoption_receipts
        ADD COLUMN authority_lease_fencing_token BLOB
            CHECK(authority_lease_fencing_token IS NULL OR length(authority_lease_fencing_token) = 32);
     ALTER TABLE task_adoption_receipts
        ADD COLUMN authority_lease_expires_at_ms INTEGER;

     CREATE TRIGGER task_adoption_authority_lease_binding_immutable
     BEFORE UPDATE ON task_adoption_receipts
     WHEN NEW.authority_lease_authority_id IS NOT OLD.authority_lease_authority_id
       OR NEW.authority_lease_holder_id IS NOT OLD.authority_lease_holder_id
       OR NEW.authority_lease_term IS NOT OLD.authority_lease_term
       OR NEW.authority_lease_epoch IS NOT OLD.authority_lease_epoch
       OR NEW.authority_lease_fencing_token IS NOT OLD.authority_lease_fencing_token
       OR NEW.authority_lease_expires_at_ms IS NOT OLD.authority_lease_expires_at_ms
     BEGIN
        SELECT RAISE(ABORT, 'authority lease adoption binding is immutable');
     END;

     PRAGMA user_version = 29;";

pub(crate) const SCHEMA_V30_SQL: &str = "CREATE TABLE task_authority_takeover_fence_receipts (
        receipt_id BLOB PRIMARY KEY NOT NULL CHECK(length(receipt_id) = 16),
        task_id BLOB NOT NULL CHECK(length(task_id) = 16),
        task_generation BLOB NOT NULL CHECK(length(task_generation) = 8),
        frozen_registry_generation BLOB NOT NULL CHECK(length(frozen_registry_generation) = 8),
        frozen_registry_root BLOB NOT NULL CHECK(length(frozen_registry_root) = 32),
        authority_lease_authority_id BLOB NOT NULL CHECK(length(authority_lease_authority_id) = 16),
        authority_lease_holder_id BLOB NOT NULL CHECK(length(authority_lease_holder_id) = 16),
        authority_lease_term BLOB NOT NULL CHECK(length(authority_lease_term) = 8),
        authority_lease_epoch BLOB NOT NULL CHECK(length(authority_lease_epoch) = 8),
        authority_lease_fencing_token BLOB NOT NULL CHECK(length(authority_lease_fencing_token) = 32),
        authority_lease_expires_at_ms INTEGER NOT NULL CHECK(authority_lease_expires_at_ms >= 0),
        control_epoch BLOB NOT NULL CHECK(length(control_epoch) = 8),
        exact_fence_set_root BLOB CHECK(exact_fence_set_root IS NULL OR length(exact_fence_set_root) = 32),
        outstanding_operation_participant_root BLOB
            CHECK(outstanding_operation_participant_root IS NULL
                  OR length(outstanding_operation_participant_root) = 32),
        created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
        UNIQUE(task_id, frozen_registry_generation, frozen_registry_root),
        FOREIGN KEY(task_id) REFERENCES tasks(task_id)
     ) STRICT;

     CREATE TRIGGER task_authority_takeover_fence_receipt_immutable
     BEFORE UPDATE ON task_authority_takeover_fence_receipts
     BEGIN
        SELECT RAISE(ABORT, 'takeover fence receipt is immutable');
     END;

     CREATE TRIGGER task_authority_takeover_fence_receipt_no_delete
     BEFORE DELETE ON task_authority_takeover_fence_receipts
     BEGIN
        SELECT RAISE(ABORT, 'takeover fence receipt is durable evidence');
     END;

     PRAGMA user_version = 30;";

pub(crate) const SCHEMA_V31_SQL: &str = "CREATE TABLE task_authority_assignments (
        assignment_id BLOB PRIMARY KEY NOT NULL CHECK(length(assignment_id) = 16),
        task_id BLOB NOT NULL CHECK(length(task_id) = 16),
        task_generation BLOB NOT NULL CHECK(length(task_generation) = 8),
        authority_id BLOB NOT NULL CHECK(length(authority_id) = 16),
        authority_lease_holder_id BLOB NOT NULL CHECK(length(authority_lease_holder_id) = 16),
        authority_lease_term BLOB NOT NULL CHECK(length(authority_lease_term) = 8),
        authority_lease_epoch BLOB NOT NULL CHECK(length(authority_lease_epoch) = 8),
        authority_lease_fencing_token BLOB NOT NULL CHECK(length(authority_lease_fencing_token) = 32),
        authority_lease_expires_at_ms INTEGER NOT NULL CHECK(authority_lease_expires_at_ms >= 0),
        control_epoch BLOB NOT NULL CHECK(length(control_epoch) = 8),
        participant_registry_generation BLOB NOT NULL CHECK(length(participant_registry_generation) = 8),
        participant_registry_root BLOB NOT NULL CHECK(length(participant_registry_root) = 32),
        assignment_state INTEGER NOT NULL CHECK(assignment_state BETWEEN 1 AND 3),
        created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
        updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= created_at_ms),
        UNIQUE(task_id, assignment_id),
        FOREIGN KEY(task_id) REFERENCES tasks(task_id)
     ) STRICT;

     CREATE TRIGGER task_authority_assignment_identity_immutable
     BEFORE UPDATE ON task_authority_assignments
     WHEN NEW.assignment_id != OLD.assignment_id
       OR NEW.task_id != OLD.task_id
       OR NEW.task_generation != OLD.task_generation
       OR NEW.authority_id != OLD.authority_id
       OR NEW.authority_lease_term != OLD.authority_lease_term
       OR NEW.created_at_ms != OLD.created_at_ms
     BEGIN
        SELECT RAISE(ABORT, 'task authority assignment identity is immutable');
     END;

     CREATE TRIGGER task_authority_assignment_no_delete
     BEFORE DELETE ON task_authority_assignments
     BEGIN
        SELECT RAISE(ABORT, 'task authority assignment is durable evidence');
     END;

     PRAGMA user_version = 31;";

pub(crate) const SCHEMA_V32_SQL: &str = "CREATE TABLE task_authority_takeover_receipts (
        receipt_id BLOB PRIMARY KEY NOT NULL CHECK(length(receipt_id) = 16),
        task_id BLOB NOT NULL CHECK(length(task_id) = 16),
        task_generation BLOB NOT NULL CHECK(length(task_generation) = 8),
        old_assignment_id BLOB NOT NULL CHECK(length(old_assignment_id) = 16),
        new_assignment_id BLOB CHECK(new_assignment_id IS NULL),
        fence_receipt_id BLOB NOT NULL CHECK(length(fence_receipt_id) = 16),
        frozen_old_authority_term BLOB NOT NULL CHECK(length(frozen_old_authority_term) = 8),
        frozen_old_control_epoch BLOB NOT NULL CHECK(length(frozen_old_control_epoch) = 8),
        new_authority_id BLOB NOT NULL CHECK(length(new_authority_id) = 16),
        new_authority_lease_holder_id BLOB NOT NULL CHECK(length(new_authority_lease_holder_id) = 16),
        new_authority_lease_term BLOB NOT NULL CHECK(length(new_authority_lease_term) = 8),
        new_authority_lease_epoch BLOB NOT NULL CHECK(length(new_authority_lease_epoch) = 8),
        new_authority_lease_fencing_token BLOB NOT NULL CHECK(length(new_authority_lease_fencing_token) = 32),
        new_authority_lease_expires_at_ms INTEGER NOT NULL CHECK(new_authority_lease_expires_at_ms >= 0),
        new_control_epoch BLOB NOT NULL CHECK(length(new_control_epoch) = 8),
        frozen_registry_generation BLOB NOT NULL CHECK(length(frozen_registry_generation) = 8),
        frozen_registry_root BLOB NOT NULL CHECK(length(frozen_registry_root) = 32),
        exact_fence_set_root BLOB CHECK(exact_fence_set_root IS NULL OR length(exact_fence_set_root) = 32),
        outstanding_operation_participant_root BLOB
            CHECK(outstanding_operation_participant_root IS NULL
                  OR length(outstanding_operation_participant_root) = 32),
        barrier_state INTEGER NOT NULL CHECK(barrier_state = 1),
        created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
        UNIQUE(task_id, old_assignment_id, fence_receipt_id),
        FOREIGN KEY(task_id) REFERENCES tasks(task_id),
        FOREIGN KEY(old_assignment_id) REFERENCES task_authority_assignments(assignment_id),
        FOREIGN KEY(fence_receipt_id) REFERENCES task_authority_takeover_fence_receipts(receipt_id)
     ) STRICT;

     CREATE TRIGGER task_authority_takeover_receipt_immutable
     BEFORE UPDATE ON task_authority_takeover_receipts
     BEGIN
        SELECT RAISE(ABORT, 'task authority takeover receipt is immutable');
     END;

     CREATE TRIGGER task_authority_takeover_receipt_no_delete
     BEFORE DELETE ON task_authority_takeover_receipts
     BEGIN
        SELECT RAISE(ABORT, 'task authority takeover receipt is durable evidence');
     END;

     PRAGMA user_version = 32;";

pub(crate) const SCHEMA_V33_SQL: &str = "CREATE TABLE task_authority_takeover_barrier_receipts (
        receipt_id BLOB PRIMARY KEY NOT NULL CHECK(length(receipt_id) = 16),
        takeover_receipt_id BLOB NOT NULL CHECK(length(takeover_receipt_id) = 16),
        task_id BLOB NOT NULL CHECK(length(task_id) = 16),
        task_generation BLOB NOT NULL CHECK(length(task_generation) = 8),
        participant_type INTEGER NOT NULL CHECK(participant_type BETWEEN 1 AND 8),
        participant_id BLOB NOT NULL CHECK(length(participant_id) = 16),
        participant_generation BLOB NOT NULL CHECK(length(participant_generation) = 8),
        admission_receipt_id BLOB NOT NULL CHECK(length(admission_receipt_id) = 16),
        remote_receipt_id BLOB NOT NULL CHECK(length(remote_receipt_id) = 16),
        fence_set_root BLOB NOT NULL CHECK(length(fence_set_root) = 32),
        barrier_state INTEGER NOT NULL CHECK(barrier_state = 1),
        observed_at_ms INTEGER NOT NULL CHECK(observed_at_ms >= 0),
        UNIQUE(takeover_receipt_id, participant_type, participant_id),
        FOREIGN KEY(takeover_receipt_id) REFERENCES task_authority_takeover_receipts(receipt_id),
        FOREIGN KEY(task_id) REFERENCES tasks(task_id)
     ) STRICT;

     CREATE TRIGGER task_authority_takeover_barrier_receipt_immutable
     BEFORE UPDATE ON task_authority_takeover_barrier_receipts
     BEGIN
        SELECT RAISE(ABORT, 'task authority takeover barrier receipt is immutable');
     END;

     CREATE TRIGGER task_authority_takeover_barrier_receipt_no_delete
     BEFORE DELETE ON task_authority_takeover_barrier_receipts
     BEGIN
        SELECT RAISE(ABORT, 'task authority takeover barrier receipt is durable evidence');
     END;

     PRAGMA user_version = 33;";

pub(crate) const SCHEMA_V34_SQL: &str = "CREATE TABLE task_authority_takeover_fence_members (
        fence_receipt_id BLOB NOT NULL CHECK(length(fence_receipt_id) = 16),
        task_id BLOB NOT NULL CHECK(length(task_id) = 16),
        task_generation BLOB NOT NULL CHECK(length(task_generation) = 8),
        participant_type INTEGER NOT NULL CHECK(participant_type BETWEEN 1 AND 8),
        participant_id BLOB NOT NULL CHECK(length(participant_id) = 16),
        participant_generation BLOB NOT NULL CHECK(length(participant_generation) = 8),
        admission_receipt_id BLOB NOT NULL CHECK(length(admission_receipt_id) = 16),
        PRIMARY KEY(fence_receipt_id, participant_type, participant_id),
        FOREIGN KEY(fence_receipt_id) REFERENCES task_authority_takeover_fence_receipts(receipt_id),
        FOREIGN KEY(task_id) REFERENCES tasks(task_id)
     ) STRICT;

     CREATE TRIGGER task_authority_takeover_fence_member_immutable
     BEFORE UPDATE ON task_authority_takeover_fence_members
     BEGIN
        SELECT RAISE(ABORT, 'task authority takeover fence member is immutable');
     END;

     CREATE TRIGGER task_authority_takeover_fence_member_no_delete
     BEFORE DELETE ON task_authority_takeover_fence_members
     BEGIN
        SELECT RAISE(ABORT, 'task authority takeover fence member is durable evidence');
     END;

     PRAGMA user_version = 34;";

pub(crate) const SCHEMA_V35_SQL: &str = "ALTER TABLE task_authority_takeover_barrier_receipts
     ADD COLUMN barrier_receipt_digest BLOB
         CHECK(barrier_receipt_digest IS NULL OR length(barrier_receipt_digest) = 32);

     PRAGMA user_version = 35;";

pub(crate) const SCHEMA_V36_SQL: &str = "ALTER TABLE task_authority_takeover_barrier_receipts
     ADD COLUMN signer_principal_id BLOB
         CHECK(signer_principal_id IS NULL OR length(signer_principal_id) = 16);
     ALTER TABLE task_authority_takeover_barrier_receipts
     ADD COLUMN signer_control_domain_id BLOB
         CHECK(signer_control_domain_id IS NULL OR length(signer_control_domain_id) = 16);
     ALTER TABLE task_authority_takeover_barrier_receipts
     ADD COLUMN signer_key_id BLOB
         CHECK(signer_key_id IS NULL OR length(signer_key_id) = 16);
     ALTER TABLE task_authority_takeover_barrier_receipts
     ADD COLUMN signer_key_generation INTEGER
         CHECK(signer_key_generation IS NULL OR signer_key_generation >= 1);
     ALTER TABLE task_authority_takeover_barrier_receipts
     ADD COLUMN signer_signature BLOB
         CHECK(signer_signature IS NULL OR length(signer_signature) = 64);

      CREATE TRIGGER task_authority_takeover_barrier_receipts_signer_coupled
      BEFORE INSERT ON task_authority_takeover_barrier_receipts
      WHEN (NEW.signer_principal_id IS NULL) + (NEW.signer_control_domain_id IS NULL)
            + (NEW.signer_key_id IS NULL) + (NEW.signer_key_generation IS NULL)
            + (NEW.signer_signature IS NULL) NOT IN (0, 5)
      BEGIN
          SELECT RAISE(ABORT, 'task authority takeover barrier signer columns must be coupled');
      END;

      PRAGMA user_version = 36;";

pub(crate) const SCHEMA_V37_SQL: &str = "DROP TRIGGER task_authority_takeover_receipt_immutable;
     DROP TRIGGER task_authority_takeover_receipt_no_delete;
     CREATE TABLE task_authority_takeover_receipts_v37 (
         receipt_id BLOB PRIMARY KEY NOT NULL CHECK(length(receipt_id) = 16),
         task_id BLOB NOT NULL CHECK(length(task_id) = 16),
         task_generation BLOB NOT NULL CHECK(length(task_generation) = 8),
         old_assignment_id BLOB NOT NULL CHECK(length(old_assignment_id) = 16),
         new_assignment_id BLOB CHECK(new_assignment_id IS NULL OR length(new_assignment_id) = 16),
         fence_receipt_id BLOB NOT NULL CHECK(length(fence_receipt_id) = 16),
         frozen_old_authority_term BLOB NOT NULL CHECK(length(frozen_old_authority_term) = 8),
         frozen_old_control_epoch BLOB NOT NULL CHECK(length(frozen_old_control_epoch) = 8),
         new_authority_id BLOB NOT NULL CHECK(length(new_authority_id) = 16),
         new_authority_lease_holder_id BLOB NOT NULL CHECK(length(new_authority_lease_holder_id) = 16),
         new_authority_lease_term BLOB NOT NULL CHECK(length(new_authority_lease_term) = 8),
         new_authority_lease_epoch BLOB NOT NULL CHECK(length(new_authority_lease_epoch) = 8),
         new_authority_lease_fencing_token BLOB NOT NULL CHECK(length(new_authority_lease_fencing_token) = 32),
         new_authority_lease_expires_at_ms INTEGER NOT NULL CHECK(new_authority_lease_expires_at_ms >= 0),
         new_control_epoch BLOB NOT NULL CHECK(length(new_control_epoch) = 8),
         frozen_registry_generation BLOB NOT NULL CHECK(length(frozen_registry_generation) = 8),
         frozen_registry_root BLOB NOT NULL CHECK(length(frozen_registry_root) = 32),
         exact_fence_set_root BLOB CHECK(exact_fence_set_root IS NULL OR length(exact_fence_set_root) = 32),
         outstanding_operation_participant_root BLOB
             CHECK(outstanding_operation_participant_root IS NULL
                   OR length(outstanding_operation_participant_root) = 32),
         barrier_state INTEGER NOT NULL CHECK(barrier_state IN (1, 2)),
         created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
         UNIQUE(task_id, old_assignment_id, fence_receipt_id),
         FOREIGN KEY(task_id) REFERENCES tasks(task_id),
         FOREIGN KEY(old_assignment_id) REFERENCES task_authority_assignments(assignment_id),
         FOREIGN KEY(fence_receipt_id) REFERENCES task_authority_takeover_fence_receipts(receipt_id)
      ) STRICT;

      INSERT INTO task_authority_takeover_receipts_v37 (
          receipt_id, task_id, task_generation, old_assignment_id,
          new_assignment_id, fence_receipt_id, frozen_old_authority_term,
          frozen_old_control_epoch, new_authority_id,
          new_authority_lease_holder_id, new_authority_lease_term,
          new_authority_lease_epoch, new_authority_lease_fencing_token,
          new_authority_lease_expires_at_ms, new_control_epoch,
          frozen_registry_generation, frozen_registry_root,
          exact_fence_set_root, outstanding_operation_participant_root,
          barrier_state, created_at_ms
      )
      SELECT receipt_id, task_id, task_generation, old_assignment_id,
             new_assignment_id, fence_receipt_id, frozen_old_authority_term,
             frozen_old_control_epoch, new_authority_id,
             new_authority_lease_holder_id, new_authority_lease_term,
             new_authority_lease_epoch, new_authority_lease_fencing_token,
             new_authority_lease_expires_at_ms, new_control_epoch,
             frozen_registry_generation, frozen_registry_root,
             exact_fence_set_root, outstanding_operation_participant_root,
             barrier_state, created_at_ms
      FROM task_authority_takeover_receipts;
      DROP TABLE task_authority_takeover_receipts;
      ALTER TABLE task_authority_takeover_receipts_v37
          RENAME TO task_authority_takeover_receipts;

      CREATE TRIGGER task_authority_takeover_receipt_immutable
      BEFORE UPDATE ON task_authority_takeover_receipts
      WHEN NOT (
          OLD.barrier_state = 1
          AND NEW.barrier_state = 2
          AND OLD.new_assignment_id IS NULL
          AND NEW.new_assignment_id IS NOT NULL
          AND NEW.receipt_id IS OLD.receipt_id
          AND NEW.task_id IS OLD.task_id
          AND NEW.task_generation IS OLD.task_generation
          AND NEW.old_assignment_id IS OLD.old_assignment_id
          AND NEW.fence_receipt_id IS OLD.fence_receipt_id
          AND NEW.frozen_old_authority_term IS OLD.frozen_old_authority_term
          AND NEW.frozen_old_control_epoch IS OLD.frozen_old_control_epoch
          AND NEW.new_authority_id IS OLD.new_authority_id
          AND NEW.new_authority_lease_holder_id IS OLD.new_authority_lease_holder_id
          AND NEW.new_authority_lease_term IS OLD.new_authority_lease_term
          AND NEW.new_authority_lease_epoch IS OLD.new_authority_lease_epoch
          AND NEW.new_authority_lease_fencing_token IS OLD.new_authority_lease_fencing_token
          AND NEW.new_authority_lease_expires_at_ms IS OLD.new_authority_lease_expires_at_ms
          AND NEW.new_control_epoch IS OLD.new_control_epoch
          AND NEW.frozen_registry_generation IS OLD.frozen_registry_generation
          AND NEW.frozen_registry_root IS OLD.frozen_registry_root
          AND NEW.exact_fence_set_root IS OLD.exact_fence_set_root
          AND NEW.outstanding_operation_participant_root IS OLD.outstanding_operation_participant_root
          AND NEW.created_at_ms IS OLD.created_at_ms
      )
      BEGIN
          SELECT RAISE(ABORT, 'task authority takeover receipt is immutable');
      END;

      CREATE TRIGGER task_authority_takeover_receipt_no_delete
      BEFORE DELETE ON task_authority_takeover_receipts
      BEGIN
          SELECT RAISE(ABORT, 'task authority takeover receipt is durable evidence');
      END;

      PRAGMA user_version = 37;";

pub(crate) const SCHEMA_V38_SQL: &str = "CREATE TABLE task_cross_term_adoption_receipts (
        receipt_id BLOB PRIMARY KEY NOT NULL CHECK(length(receipt_id) = 16),
        task_id BLOB NOT NULL CHECK(length(task_id) = 16),
        task_generation BLOB NOT NULL CHECK(length(task_generation) = 8),
        idempotency_key BLOB NOT NULL CHECK(length(idempotency_key) = 16),
        original_permit_id BLOB NOT NULL CHECK(length(original_permit_id) = 16),
        original_permit_epoch BLOB NOT NULL CHECK(length(original_permit_epoch) = 8),
        original_control_epoch BLOB NOT NULL CHECK(length(original_control_epoch) = 8),
        original_cancel_epoch BLOB NOT NULL CHECK(length(original_cancel_epoch) = 8),
        original_registry_generation BLOB NOT NULL CHECK(length(original_registry_generation) = 8),
        original_registry_root BLOB NOT NULL CHECK(length(original_registry_root) = 32),
        effect_set_root BLOB NOT NULL CHECK(length(effect_set_root) = 32),
        observed_effect_slot_state_root BLOB NOT NULL CHECK(length(observed_effect_slot_state_root) = 32),
        adoption_epoch BLOB NOT NULL CHECK(length(adoption_epoch) = 8),
        takeover_receipt_id BLOB NOT NULL CHECK(length(takeover_receipt_id) = 16),
        current_assignment_id BLOB NOT NULL CHECK(length(current_assignment_id) = 16),
        original_authority_id BLOB NOT NULL CHECK(length(original_authority_id) = 16),
        original_authority_holder_id BLOB NOT NULL CHECK(length(original_authority_holder_id) = 16),
        original_authority_term BLOB NOT NULL CHECK(length(original_authority_term) = 8),
        original_authority_lease_epoch BLOB NOT NULL CHECK(length(original_authority_lease_epoch) = 8),
        original_authority_fencing_token BLOB NOT NULL CHECK(length(original_authority_fencing_token) = 32),
        original_authority_lease_expires_at_ms INTEGER NOT NULL CHECK(original_authority_lease_expires_at_ms >= 0),
        current_authority_id BLOB NOT NULL CHECK(length(current_authority_id) = 16),
        current_authority_holder_id BLOB NOT NULL CHECK(length(current_authority_holder_id) = 16),
        current_authority_term BLOB NOT NULL CHECK(length(current_authority_term) = 8),
        current_authority_lease_epoch BLOB NOT NULL CHECK(length(current_authority_lease_epoch) = 8),
        current_authority_fencing_token BLOB NOT NULL CHECK(length(current_authority_fencing_token) = 32),
        current_authority_lease_expires_at_ms INTEGER NOT NULL CHECK(current_authority_lease_expires_at_ms >= 0),
        current_control_epoch BLOB NOT NULL CHECK(length(current_control_epoch) = 8),
        current_cancel_epoch BLOB NOT NULL CHECK(length(current_cancel_epoch) = 8),
        current_registry_generation BLOB NOT NULL CHECK(length(current_registry_generation) = 8),
        current_registry_root BLOB NOT NULL CHECK(length(current_registry_root) = 32),
        exact_fenced_participant_root BLOB NOT NULL CHECK(length(exact_fenced_participant_root) = 32),
        created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
        UNIQUE(task_id, idempotency_key),
        FOREIGN KEY(task_id) REFERENCES tasks(task_id),
        FOREIGN KEY(takeover_receipt_id) REFERENCES task_authority_takeover_receipts(receipt_id),
        FOREIGN KEY(current_assignment_id) REFERENCES task_authority_assignments(assignment_id)
    ) STRICT;

    CREATE INDEX task_cross_term_adoption_receipts_by_permit
        ON task_cross_term_adoption_receipts(task_id, original_permit_id);

    CREATE TRIGGER task_cross_term_adoption_receipt_immutable
    BEFORE UPDATE ON task_cross_term_adoption_receipts
    BEGIN
        SELECT RAISE(ABORT, 'cross-term adoption receipt is immutable');
    END;

    CREATE TRIGGER task_cross_term_adoption_receipt_no_delete
    BEFORE DELETE ON task_cross_term_adoption_receipts
    BEGIN
        SELECT RAISE(ABORT, 'cross-term adoption receipt is durable evidence');
    END;

    PRAGMA user_version = 38;";

const SCHEMA_V12_SQL: &str = "ALTER TABLE effect_permits
        ADD COLUMN participant_registry_generation BLOB
        CHECK(participant_registry_generation IS NULL OR length(participant_registry_generation) = 8);
    ALTER TABLE effect_permits
        ADD COLUMN participant_registry_root BLOB
        CHECK(participant_registry_root IS NULL OR length(participant_registry_root) = 32);
    ALTER TABLE task_receipts
        ADD COLUMN participant_registry_generation BLOB
        CHECK(participant_registry_generation IS NULL OR length(participant_registry_generation) = 8);
    ALTER TABLE task_receipts
        ADD COLUMN participant_registry_root BLOB
        CHECK(participant_registry_root IS NULL OR length(participant_registry_root) = 32);
    CREATE TRIGGER effect_permit_participant_binding_immutable
    BEFORE UPDATE ON effect_permits
    WHEN NEW.participant_registry_generation IS NOT OLD.participant_registry_generation
      OR NEW.participant_registry_root IS NOT OLD.participant_registry_root
    BEGIN SELECT RAISE(ABORT, 'effect permit participant binding is immutable'); END;
    PRAGMA user_version = 12;";

const SCHEMA_V10_SQL: &str = "CREATE TABLE task_snapshot_receipts (
        receipt_id BLOB PRIMARY KEY NOT NULL CHECK(length(receipt_id) = 16),
        task_id BLOB NOT NULL CHECK(length(task_id) = 16),
        snapshot_id BLOB NOT NULL CHECK(length(snapshot_id) = 16),
        builder_id BLOB NOT NULL CHECK(length(builder_id) = 16),
        builder_version_digest BLOB NOT NULL CHECK(length(builder_version_digest) = 32),
        dependency_closure_root BLOB NOT NULL CHECK(length(dependency_closure_root) = 32),
        semantic_resolver_digest BLOB NOT NULL CHECK(length(semantic_resolver_digest) = 32),
        canonical_iteration_digest BLOB NOT NULL CHECK(length(canonical_iteration_digest) = 32),
        achieved_consistency INTEGER NOT NULL CHECK(achieved_consistency BETWEEN 0 AND 3),
        durability INTEGER NOT NULL DEFAULT 1 CHECK(durability = 1),
        built_at_ms INTEGER NOT NULL CHECK(built_at_ms >= 0),
        authority_id BLOB NOT NULL CHECK(length(authority_id) = 16),
        key_id BLOB NOT NULL CHECK(length(key_id) = 16),
        signature BLOB NOT NULL CHECK(length(signature) = 64),
        UNIQUE(task_id, snapshot_id),
        FOREIGN KEY(task_id, snapshot_id) REFERENCES task_snapshots(task_id, snapshot_id)
    ) STRICT;

    CREATE TABLE task_snapshot_checkpoint_receipts (
        snapshot_receipt_id BLOB NOT NULL CHECK(length(snapshot_receipt_id) = 16),
        checkpoint_seq INTEGER NOT NULL CHECK(checkpoint_seq >= 0),
        checkpoint_receipt_id BLOB NOT NULL CHECK(length(checkpoint_receipt_id) = 16),
        PRIMARY KEY(snapshot_receipt_id, checkpoint_seq),
        UNIQUE(snapshot_receipt_id, checkpoint_receipt_id),
        FOREIGN KEY(snapshot_receipt_id) REFERENCES task_snapshot_receipts(receipt_id)
    ) STRICT;

    CREATE TRIGGER task_snapshot_receipt_is_immutable
    BEFORE UPDATE ON task_snapshot_receipts
    BEGIN SELECT RAISE(ABORT, 'task snapshot receipt is immutable'); END;

    CREATE TRIGGER task_snapshot_checkpoint_receipt_is_immutable
    BEFORE UPDATE ON task_snapshot_checkpoint_receipts
    BEGIN SELECT RAISE(ABORT, 'task snapshot checkpoint receipt is immutable'); END;

    ALTER TABLE task_attempts ADD COLUMN snapshot_receipt_id BLOB
        CHECK(snapshot_receipt_id IS NULL OR length(snapshot_receipt_id) = 16)
        REFERENCES task_snapshot_receipts(receipt_id);

    PRAGMA user_version = 10;";

const SCHEMA_V5_SQL: &str =
    "ALTER TABLE commit_permits ADD COLUMN group_id BLOB CHECK(group_id IS NULL OR length(group_id) = 16);
     ALTER TABLE commit_permits ADD COLUMN membership_generation BLOB CHECK(membership_generation IS NULL OR length(membership_generation) = 8);
     ALTER TABLE commit_permits ADD COLUMN membership_root BLOB CHECK(membership_root IS NULL OR length(membership_root) = 32);
     ALTER TABLE commit_permits ADD COLUMN group_policy_digest BLOB CHECK(group_policy_digest IS NULL OR length(group_policy_digest) = 32);
     ALTER TABLE task_receipts ADD COLUMN group_id BLOB CHECK(group_id IS NULL OR length(group_id) = 16);
     ALTER TABLE task_receipts ADD COLUMN membership_generation BLOB CHECK(membership_generation IS NULL OR length(membership_generation) = 8);
     ALTER TABLE task_receipts ADD COLUMN membership_root BLOB CHECK(membership_root IS NULL OR length(membership_root) = 32);
     ALTER TABLE task_receipts ADD COLUMN group_policy_digest BLOB CHECK(group_policy_digest IS NULL OR length(group_policy_digest) = 32);
     PRAGMA user_version = 5;";

const SCHEMA_V1_SQL: &str =
    "CREATE TABLE tasks (
            task_id BLOB PRIMARY KEY NOT NULL CHECK(length(task_id) = 16),
            task_generation BLOB NOT NULL CHECK(length(task_generation) = 8),
            head_commit_seq BLOB NOT NULL CHECK(length(head_commit_seq) = 8),
            head_effect_history_root BLOB NOT NULL CHECK(length(head_effect_history_root) = 32),
            retry_fence_epoch BLOB NOT NULL CHECK(length(retry_fence_epoch) = 8),
            control_epoch BLOB NOT NULL CHECK(length(control_epoch) = 8),
            cancel_epoch BLOB NOT NULL CHECK(length(cancel_epoch) = 8),
            permit_epoch BLOB NOT NULL CHECK(length(permit_epoch) = 8),
            task_state INTEGER NOT NULL,
            revision INTEGER NOT NULL DEFAULT 0,
            created_at_ms INTEGER NOT NULL,
            updated_at_ms INTEGER NOT NULL
        ) STRICT;

        CREATE TABLE task_snapshots (
            task_id BLOB NOT NULL CHECK(length(task_id) = 16),
            snapshot_id BLOB NOT NULL CHECK(length(snapshot_id) = 16),
            snapshot_digest BLOB NOT NULL CHECK(length(snapshot_digest) = 32),
            expected_head_commit_seq BLOB NOT NULL CHECK(length(expected_head_commit_seq) = 8),
            effect_history_root BLOB NOT NULL CHECK(length(effect_history_root) = 32),
            retry_fence_epoch BLOB NOT NULL CHECK(length(retry_fence_epoch) = 8),
            created_at_ms INTEGER NOT NULL,
            PRIMARY KEY(task_id, snapshot_id),
            FOREIGN KEY(task_id) REFERENCES tasks(task_id)
        ) STRICT;

        CREATE TRIGGER task_snapshot_is_immutable
        BEFORE UPDATE ON task_snapshots
        BEGIN
            SELECT RAISE(ABORT, 'task snapshot is immutable');
        END;

        CREATE TABLE task_attempts (
            attempt_id BLOB PRIMARY KEY NOT NULL CHECK(length(attempt_id) = 16),
            task_id BLOB NOT NULL CHECK(length(task_id) = 16),
            attempt_generation BLOB NOT NULL CHECK(length(attempt_generation) = 8),
            snapshot_id BLOB NOT NULL CHECK(length(snapshot_id) = 16),
            cancellation_scope_id BLOB NOT NULL CHECK(length(cancellation_scope_id) = 16),
            cancellation_generation BLOB NOT NULL CHECK(length(cancellation_generation) = 8),
            idempotency_key BLOB NOT NULL CHECK(length(idempotency_key) = 16),
            attempt_state INTEGER NOT NULL,
            receipt_id BLOB CHECK(receipt_id IS NULL OR length(receipt_id) = 16),
            created_at_ms INTEGER NOT NULL,
            updated_at_ms INTEGER NOT NULL,
            UNIQUE(task_id, idempotency_key),
            FOREIGN KEY(task_id) REFERENCES tasks(task_id)
        ) STRICT;

        CREATE TABLE commit_permits (
            permit_id BLOB PRIMARY KEY NOT NULL CHECK(length(permit_id) = 16),
            task_id BLOB NOT NULL CHECK(length(task_id) = 16),
            idempotency_key BLOB NOT NULL CHECK(length(idempotency_key) = 16),
            attempt_id BLOB NOT NULL CHECK(length(attempt_id) = 16),
            attempt_generation BLOB NOT NULL CHECK(length(attempt_generation) = 8),
            expected_head_commit_seq BLOB NOT NULL CHECK(length(expected_head_commit_seq) = 8),
            expected_effect_history_root BLOB NOT NULL CHECK(length(expected_effect_history_root) = 32),
            expected_retry_fence_epoch BLOB NOT NULL CHECK(length(expected_retry_fence_epoch) = 8),
            write_set_root BLOB NOT NULL CHECK(length(write_set_root) = 32),
            permit_epoch BLOB NOT NULL CHECK(length(permit_epoch) = 8),
            control_epoch BLOB NOT NULL CHECK(length(control_epoch) = 8),
            cancel_epoch BLOB NOT NULL CHECK(length(cancel_epoch) = 8),
            valid_until_ms INTEGER NOT NULL,
            permit_state INTEGER NOT NULL,
            created_at_ms INTEGER NOT NULL,
            updated_at_ms INTEGER NOT NULL,
            UNIQUE(task_id, idempotency_key),
            FOREIGN KEY(task_id) REFERENCES tasks(task_id)
        ) STRICT;

        -- Defense in depth: the single-writer transaction already
        -- serializes issuance; this index makes a second outstanding
        -- permit per task unrepresentable on disk.
        CREATE UNIQUE INDEX commit_permits_single_active
            ON commit_permits(task_id) WHERE permit_state = 0;

        CREATE TABLE task_cancels (
            task_id BLOB PRIMARY KEY NOT NULL CHECK(length(task_id) = 16),
            idempotency_key BLOB NOT NULL CHECK(length(idempotency_key) = 16),
            cancel_epoch_after BLOB NOT NULL CHECK(length(cancel_epoch_after) = 8),
            created_at_ms INTEGER NOT NULL,
            FOREIGN KEY(task_id) REFERENCES tasks(task_id)
        ) STRICT;

        CREATE TABLE task_receipts (
            receipt_id BLOB PRIMARY KEY NOT NULL CHECK(length(receipt_id) = 16),
            task_id BLOB NOT NULL CHECK(length(task_id) = 16),
            permit_id BLOB CHECK(permit_id IS NULL OR length(permit_id) = 16),
            attempt_id BLOB NOT NULL CHECK(length(attempt_id) = 16),
            attempt_generation BLOB NOT NULL CHECK(length(attempt_generation) = 8),
            outcome INTEGER NOT NULL,
            prior_head_commit_seq BLOB NOT NULL CHECK(length(prior_head_commit_seq) = 8),
            prior_effect_history_root BLOB NOT NULL CHECK(length(prior_effect_history_root) = 32),
            prior_retry_fence_epoch BLOB NOT NULL CHECK(length(prior_retry_fence_epoch) = 8),
            new_head_commit_seq BLOB NOT NULL CHECK(length(new_head_commit_seq) = 8),
            new_effect_history_root BLOB NOT NULL CHECK(length(new_effect_history_root) = 32),
            new_retry_fence_epoch BLOB NOT NULL CHECK(length(new_retry_fence_epoch) = 8),
            created_at_ms INTEGER NOT NULL,
            FOREIGN KEY(task_id) REFERENCES tasks(task_id)
        ) STRICT;

        CREATE INDEX task_receipts_by_permit
            ON task_receipts(task_id, permit_id);

        CREATE TRIGGER task_receipt_is_immutable
        BEFORE UPDATE ON task_receipts
        BEGIN
            SELECT RAISE(ABORT, 'task receipt is immutable');
        END;

        PRAGMA user_version = 1;";

pub(crate) const SCHEMA_V39_SQL: &str = "CREATE TABLE task_resource_cost_receipts (
        task_receipt_id BLOB NOT NULL CHECK(length(task_receipt_id) = 16),
        task_id BLOB NOT NULL CHECK(length(task_id) = 16),
        reservation_id BLOB NOT NULL CHECK(length(reservation_id) = 16),
        account_id BLOB NOT NULL CHECK(length(account_id) = 16),
        quote_id BLOB NOT NULL CHECK(length(quote_id) = 16),
        call_id BLOB NOT NULL CHECK(length(call_id) = 16),
        operation_id BLOB NOT NULL CHECK(length(operation_id) = 16),
        upper_bound BLOB NOT NULL CHECK(length(upper_bound) = 8),
        activation_receipt_id BLOB NOT NULL CHECK(length(activation_receipt_id) = 16),
        activated_at_ms BLOB NOT NULL CHECK(length(activated_at_ms) = 8),
        finalization_receipt_id BLOB NOT NULL CHECK(length(finalization_receipt_id) = 16),
        effect_closed_proof_digest BLOB NOT NULL CHECK(length(effect_closed_proof_digest) = 32),
        high_water_seq BLOB NOT NULL CHECK(length(high_water_seq) = 8),
        final_seq BLOB NOT NULL CHECK(length(final_seq) = 8),
        high_water BLOB NOT NULL CHECK(length(high_water) = 8),
        final_usage BLOB NOT NULL CHECK(length(final_usage) = 8),
        refund_credit BLOB NOT NULL CHECK(length(refund_credit) = 8),
        finalized_at_ms BLOB NOT NULL CHECK(length(finalized_at_ms) = 8),
        PRIMARY KEY(task_receipt_id, reservation_id),
        FOREIGN KEY(task_receipt_id) REFERENCES task_receipts(receipt_id),
        FOREIGN KEY(task_id) REFERENCES tasks(task_id)
    ) STRICT;

    CREATE INDEX task_resource_cost_receipts_by_task
        ON task_resource_cost_receipts(task_id, task_receipt_id);

    CREATE TABLE task_resource_cost_consumptions (
        task_receipt_id BLOB NOT NULL CHECK(length(task_receipt_id) = 16),
        reservation_id BLOB NOT NULL CHECK(length(reservation_id) = 16),
        sequence BLOB NOT NULL CHECK(length(sequence) = 8),
        receipt_id BLOB NOT NULL CHECK(length(receipt_id) = 16),
        cumulative_usage BLOB NOT NULL CHECK(length(cumulative_usage) = 8),
        consumed_at_ms BLOB NOT NULL CHECK(length(consumed_at_ms) = 8),
        PRIMARY KEY(task_receipt_id, reservation_id, sequence),
        FOREIGN KEY(task_receipt_id, reservation_id)
            REFERENCES task_resource_cost_receipts(task_receipt_id, reservation_id)
    ) STRICT;

    CREATE TRIGGER task_resource_cost_receipt_immutable
    BEFORE UPDATE ON task_resource_cost_receipts
    BEGIN
        SELECT RAISE(ABORT, 'nested Resource cost receipt is immutable');
    END;

    CREATE TRIGGER task_resource_cost_receipt_no_delete
    BEFORE DELETE ON task_resource_cost_receipts
    BEGIN
        SELECT RAISE(ABORT, 'nested Resource cost receipt is durable evidence');
    END;

    CREATE TRIGGER task_resource_cost_consumption_immutable
    BEFORE UPDATE ON task_resource_cost_consumptions
    BEGIN
        SELECT RAISE(ABORT, 'nested Resource cost consumption is immutable');
    END;

    CREATE TRIGGER task_resource_cost_consumption_no_delete
    BEFORE DELETE ON task_resource_cost_consumptions
    BEGIN
        SELECT RAISE(ABORT, 'nested Resource cost consumption is durable evidence');
    END;

    PRAGMA user_version = 39;";
