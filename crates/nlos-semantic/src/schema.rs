use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

use crate::SemanticAuthorityError;

#[allow(clippy::too_many_lines)] // One auditable transaction contains the complete v2 DDL.
pub(crate) fn migrate_v2(connection: &mut Connection) -> Result<(), SemanticAuthorityError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE TABLE content_objects (
            content_digest BLOB PRIMARY KEY NOT NULL CHECK(length(content_digest) = 32),
            media_type TEXT NOT NULL CHECK(length(media_type) BETWEEN 1 AND 128),
            exact_bytes BLOB NOT NULL CHECK(length(exact_bytes) <= 1048576)
        ) STRICT;

        CREATE TABLE spec_bodies (
            spec_body_digest BLOB PRIMARY KEY NOT NULL CHECK(length(spec_body_digest) = 32),
            canonical_spec_body BLOB NOT NULL CHECK(length(canonical_spec_body) <= 65536)
        ) STRICT;

        CREATE TABLE semantic_events (
            event_id BLOB PRIMARY KEY NOT NULL CHECK(length(event_id) = 32),
            canonical_unsigned_event BLOB NOT NULL CHECK(length(canonical_unsigned_event) <= 65536),
            event_type INTEGER NOT NULL CHECK(event_type IN (1, 5)),
            scope_kind INTEGER NOT NULL CHECK(scope_kind IN (1, 2)),
            scope_id BLOB NOT NULL CHECK(length(scope_id) = 16),
            issuer_principal_id BLOB NOT NULL CHECK(length(issuer_principal_id) = 16),
            issuer_process_id BLOB NOT NULL CHECK(length(issuer_process_id) = 16),
            issuer_process_generation INTEGER NOT NULL CHECK(issuer_process_generation >= 1),
            control_domain_id BLOB NOT NULL CHECK(length(control_domain_id) = 16),
            issued_at_unix_ns INTEGER NOT NULL CHECK(issued_at_unix_ns >= 0),
            valid_until_ms INTEGER CHECK(valid_until_ms IS NULL OR valid_until_ms >= 0),
            purpose_digest BLOB CHECK(purpose_digest IS NULL OR length(purpose_digest) = 32),
            key_id BLOB NOT NULL CHECK(length(key_id) = 16),
            content_digest BLOB CHECK(content_digest IS NULL OR length(content_digest) = 32),
            spec_body_digest BLOB CHECK(spec_body_digest IS NULL OR length(spec_body_digest) = 32),
            FOREIGN KEY(content_digest) REFERENCES content_objects(content_digest),
            FOREIGN KEY(spec_body_digest) REFERENCES spec_bodies(spec_body_digest),
            CHECK((event_type = 1 AND content_digest IS NOT NULL AND spec_body_digest IS NULL)
               OR (event_type = 5 AND content_digest IS NULL AND spec_body_digest IS NOT NULL))
        ) STRICT;

        CREATE TABLE event_signatures (
            event_id BLOB PRIMARY KEY NOT NULL CHECK(length(event_id) = 32),
            key_id BLOB NOT NULL CHECK(length(key_id) = 16),
            signature BLOB NOT NULL CHECK(length(signature) = 64),
            FOREIGN KEY(event_id) REFERENCES semantic_events(event_id)
        ) STRICT;

        CREATE TABLE event_log (
            log_seq INTEGER PRIMARY KEY AUTOINCREMENT,
            event_id BLOB NOT NULL UNIQUE CHECK(length(event_id) = 32),
            FOREIGN KEY(event_id) REFERENCES semantic_events(event_id)
        ) STRICT;

        CREATE TABLE lineage_edges (
            child_event_id BLOB NOT NULL CHECK(length(child_event_id) = 32),
            parent_event_id BLOB NOT NULL CHECK(length(parent_event_id) = 32),
            edge_kind INTEGER NOT NULL CHECK(edge_kind IN (1, 2)),
            PRIMARY KEY(child_event_id, parent_event_id, edge_kind),
            FOREIGN KEY(child_event_id) REFERENCES semantic_events(event_id),
            FOREIGN KEY(parent_event_id) REFERENCES semantic_events(event_id),
            CHECK(child_event_id != parent_event_id)
        ) STRICT;

        CREATE TABLE admission_receipts (
            receipt_id BLOB PRIMARY KEY NOT NULL CHECK(length(receipt_id) = 16),
            event_id BLOB NOT NULL UNIQUE CHECK(length(event_id) = 32),
            log_seq INTEGER NOT NULL UNIQUE CHECK(log_seq >= 1),
            admitted_at_ms INTEGER NOT NULL CHECK(admitted_at_ms >= 0),
            effective_valid_until_ms INTEGER CHECK(effective_valid_until_ms IS NULL OR effective_valid_until_ms >= admitted_at_ms),
            effective_taint INTEGER NOT NULL CHECK(effective_taint BETWEEN 0 AND 7),
            authz_policy_digest BLOB NOT NULL CHECK(length(authz_policy_digest) = 32),
            durability INTEGER NOT NULL CHECK(durability = 2),
            store_principal_id BLOB NOT NULL CHECK(length(store_principal_id) = 16),
            store_control_domain_id BLOB NOT NULL CHECK(length(store_control_domain_id) = 16),
            store_key_id BLOB NOT NULL CHECK(length(store_key_id) = 16),
            store_signature BLOB NOT NULL CHECK(length(store_signature) = 64),
            FOREIGN KEY(event_id) REFERENCES semantic_events(event_id),
            FOREIGN KEY(log_seq) REFERENCES event_log(log_seq)
        ) STRICT;

        CREATE TABLE durability_receipts (
            receipt_id BLOB PRIMARY KEY NOT NULL CHECK(length(receipt_id) = 16),
            event_id BLOB NOT NULL CHECK(length(event_id) = 32),
            durable_checkpoint_id BLOB NOT NULL CHECK(length(durable_checkpoint_id) = 32),
            durable_at_ms INTEGER NOT NULL CHECK(durable_at_ms >= 0),
            store_signature BLOB NOT NULL CHECK(length(store_signature) = 64),
            FOREIGN KEY(event_id) REFERENCES semantic_events(event_id)
        ) STRICT;

        CREATE TABLE semantic_outbox (
            log_seq INTEGER PRIMARY KEY NOT NULL CHECK(log_seq >= 1),
            event_id BLOB NOT NULL UNIQUE CHECK(length(event_id) = 32),
            receipt_id BLOB NOT NULL UNIQUE CHECK(length(receipt_id) = 16),
            acknowledged_at_ms INTEGER CHECK(acknowledged_at_ms IS NULL OR acknowledged_at_ms >= 0),
            FOREIGN KEY(log_seq) REFERENCES event_log(log_seq),
            FOREIGN KEY(event_id) REFERENCES semantic_events(event_id),
            FOREIGN KEY(receipt_id) REFERENCES admission_receipts(receipt_id)
        ) STRICT;

        CREATE TRIGGER content_objects_immutable_update BEFORE UPDATE ON content_objects
        BEGIN SELECT RAISE(ABORT, 'content object is immutable'); END;
        CREATE TRIGGER content_objects_immutable_delete BEFORE DELETE ON content_objects
        BEGIN SELECT RAISE(ABORT, 'content object is immutable'); END;
        CREATE TRIGGER spec_bodies_immutable_update BEFORE UPDATE ON spec_bodies
        BEGIN SELECT RAISE(ABORT, 'spec body is immutable'); END;
        CREATE TRIGGER spec_bodies_immutable_delete BEFORE DELETE ON spec_bodies
        BEGIN SELECT RAISE(ABORT, 'spec body is immutable'); END;
        CREATE TRIGGER semantic_events_immutable_update BEFORE UPDATE ON semantic_events
        BEGIN SELECT RAISE(ABORT, 'semantic event is immutable'); END;
        CREATE TRIGGER semantic_events_immutable_delete BEFORE DELETE ON semantic_events
        BEGIN SELECT RAISE(ABORT, 'semantic event is immutable'); END;
        CREATE TRIGGER event_signatures_immutable_update BEFORE UPDATE ON event_signatures
        BEGIN SELECT RAISE(ABORT, 'event signature is immutable'); END;
        CREATE TRIGGER event_signatures_immutable_delete BEFORE DELETE ON event_signatures
        BEGIN SELECT RAISE(ABORT, 'event signature is immutable'); END;
        CREATE TRIGGER event_log_immutable_update BEFORE UPDATE ON event_log
        BEGIN SELECT RAISE(ABORT, 'event log is immutable'); END;
        CREATE TRIGGER event_log_immutable_delete BEFORE DELETE ON event_log
        BEGIN SELECT RAISE(ABORT, 'event log is immutable'); END;
        CREATE TRIGGER lineage_edges_immutable_update BEFORE UPDATE ON lineage_edges
        BEGIN SELECT RAISE(ABORT, 'lineage edge is immutable'); END;
        CREATE TRIGGER lineage_edges_immutable_delete BEFORE DELETE ON lineage_edges
        BEGIN SELECT RAISE(ABORT, 'lineage edge is immutable'); END;
        CREATE TRIGGER admission_receipts_immutable_update BEFORE UPDATE ON admission_receipts
        BEGIN SELECT RAISE(ABORT, 'admission receipt is immutable'); END;
        CREATE TRIGGER admission_receipts_immutable_delete BEFORE DELETE ON admission_receipts
        BEGIN SELECT RAISE(ABORT, 'admission receipt is immutable'); END;
        CREATE TRIGGER durability_receipts_immutable_update BEFORE UPDATE ON durability_receipts
        BEGIN SELECT RAISE(ABORT, 'durability receipt is immutable'); END;
        CREATE TRIGGER durability_receipts_immutable_delete BEFORE DELETE ON durability_receipts
        BEGIN SELECT RAISE(ABORT, 'durability receipt is immutable'); END;
        CREATE TRIGGER semantic_outbox_identity_immutable BEFORE UPDATE ON semantic_outbox
        WHEN NEW.log_seq != OLD.log_seq OR NEW.event_id != OLD.event_id OR NEW.receipt_id != OLD.receipt_id
        BEGIN SELECT RAISE(ABORT, 'semantic outbox identity is immutable'); END;
        CREATE TRIGGER semantic_outbox_no_delete BEFORE DELETE ON semantic_outbox
        BEGIN SELECT RAISE(ABORT, 'semantic outbox history is immutable'); END;

        PRAGMA user_version = 2;",
    )?;
    transaction.commit()?;
    Ok(())
}

#[allow(clippy::too_many_lines)] // Rebuild preserves the v1 authority rows and foreign-key name.
pub(crate) fn migrate_v1_to_v2(connection: &mut Connection) -> Result<(), SemanticAuthorityError> {
    connection.pragma_update(None, "foreign_keys", "OFF")?;
    connection.pragma_update(None, "legacy_alter_table", "ON")?;
    let migration = (|| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(
            "CREATE TABLE spec_bodies (
                spec_body_digest BLOB PRIMARY KEY NOT NULL CHECK(length(spec_body_digest) = 32),
                canonical_spec_body BLOB NOT NULL CHECK(length(canonical_spec_body) <= 65536)
            ) STRICT;

            DROP TRIGGER semantic_events_immutable_update;
            DROP TRIGGER semantic_events_immutable_delete;
            ALTER TABLE semantic_events RENAME TO semantic_events_v1;

            CREATE TABLE semantic_events (
                event_id BLOB PRIMARY KEY NOT NULL CHECK(length(event_id) = 32),
                canonical_unsigned_event BLOB NOT NULL CHECK(length(canonical_unsigned_event) <= 65536),
                event_type INTEGER NOT NULL CHECK(event_type IN (1, 5)),
                scope_kind INTEGER NOT NULL CHECK(scope_kind IN (1, 2)),
                scope_id BLOB NOT NULL CHECK(length(scope_id) = 16),
                issuer_principal_id BLOB NOT NULL CHECK(length(issuer_principal_id) = 16),
                issuer_process_id BLOB NOT NULL CHECK(length(issuer_process_id) = 16),
                issuer_process_generation INTEGER NOT NULL CHECK(issuer_process_generation >= 1),
                control_domain_id BLOB NOT NULL CHECK(length(control_domain_id) = 16),
                issued_at_unix_ns INTEGER NOT NULL CHECK(issued_at_unix_ns >= 0),
                valid_until_ms INTEGER CHECK(valid_until_ms IS NULL OR valid_until_ms >= 0),
                purpose_digest BLOB CHECK(purpose_digest IS NULL OR length(purpose_digest) = 32),
                key_id BLOB NOT NULL CHECK(length(key_id) = 16),
                content_digest BLOB CHECK(content_digest IS NULL OR length(content_digest) = 32),
                spec_body_digest BLOB CHECK(spec_body_digest IS NULL OR length(spec_body_digest) = 32),
                FOREIGN KEY(content_digest) REFERENCES content_objects(content_digest),
                FOREIGN KEY(spec_body_digest) REFERENCES spec_bodies(spec_body_digest),
                CHECK((event_type = 1 AND content_digest IS NOT NULL AND spec_body_digest IS NULL)
                   OR (event_type = 5 AND content_digest IS NULL AND spec_body_digest IS NOT NULL))
            ) STRICT;

            INSERT INTO semantic_events (
                event_id, canonical_unsigned_event, event_type, scope_kind, scope_id,
                issuer_principal_id, issuer_process_id, issuer_process_generation,
                control_domain_id, issued_at_unix_ns, valid_until_ms, purpose_digest,
                key_id, content_digest, spec_body_digest
            )
            SELECT event_id, canonical_unsigned_event, event_type, scope_kind, scope_id,
                   issuer_principal_id, issuer_process_id, issuer_process_generation,
                   control_domain_id, issued_at_unix_ns, valid_until_ms, purpose_digest,
                   key_id, content_digest, NULL
            FROM semantic_events_v1;

            DROP TABLE semantic_events_v1;

            CREATE TRIGGER spec_bodies_immutable_update BEFORE UPDATE ON spec_bodies
            BEGIN SELECT RAISE(ABORT, 'spec body is immutable'); END;
            CREATE TRIGGER spec_bodies_immutable_delete BEFORE DELETE ON spec_bodies
            BEGIN SELECT RAISE(ABORT, 'spec body is immutable'); END;
            CREATE TRIGGER semantic_events_immutable_update BEFORE UPDATE ON semantic_events
            BEGIN SELECT RAISE(ABORT, 'semantic event is immutable'); END;
            CREATE TRIGGER semantic_events_immutable_delete BEFORE DELETE ON semantic_events
            BEGIN SELECT RAISE(ABORT, 'semantic event is immutable'); END;

            PRAGMA user_version = 2;",
        )?;
        let foreign_key_failure: Option<String> = transaction
            .query_row("PRAGMA foreign_key_check", [], |row| row.get(0))
            .optional()?;
        if foreign_key_failure.is_some() {
            return Err(SemanticAuthorityError::CorruptRecord(
                "foreign key violation after semantic v2 migration",
            ));
        }
        transaction.commit()?;
        Ok(())
    })();
    connection.pragma_update(None, "legacy_alter_table", "OFF")?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    migration
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::migrate_v1_to_v2;

    #[test]
    fn v1_migration_preserves_assertion_rows_and_adds_spec_storage() {
        let mut connection = Connection::open_in_memory().unwrap();
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .unwrap();
        connection
            .execute_batch(
                "CREATE TABLE content_objects (
                    content_digest BLOB PRIMARY KEY NOT NULL,
                    media_type TEXT NOT NULL,
                    exact_bytes BLOB NOT NULL
                ) STRICT;
                CREATE TABLE semantic_events (
                    event_id BLOB PRIMARY KEY NOT NULL,
                    canonical_unsigned_event BLOB NOT NULL,
                    event_type INTEGER NOT NULL CHECK(event_type = 1),
                    scope_kind INTEGER NOT NULL,
                    scope_id BLOB NOT NULL,
                    issuer_principal_id BLOB NOT NULL,
                    issuer_process_id BLOB NOT NULL,
                    issuer_process_generation INTEGER NOT NULL,
                    control_domain_id BLOB NOT NULL,
                    issued_at_unix_ns INTEGER NOT NULL,
                    valid_until_ms INTEGER,
                    purpose_digest BLOB,
                    key_id BLOB NOT NULL,
                    content_digest BLOB NOT NULL REFERENCES content_objects(content_digest)
                ) STRICT;
                CREATE TRIGGER semantic_events_immutable_update BEFORE UPDATE ON semantic_events
                BEGIN SELECT RAISE(ABORT, 'semantic event is immutable'); END;
                CREATE TRIGGER semantic_events_immutable_delete BEFORE DELETE ON semantic_events
                BEGIN SELECT RAISE(ABORT, 'semantic event is immutable'); END;
                INSERT INTO content_objects VALUES (zeroblob(32), 'text/plain', X'01');
                INSERT INTO semantic_events VALUES (
                    zeroblob(32), X'01', 1, 1, zeroblob(16), zeroblob(16),
                    zeroblob(16), 1, zeroblob(16), 1, NULL, NULL,
                    zeroblob(16), zeroblob(32)
                );
                PRAGMA user_version = 1;",
            )
            .unwrap();

        migrate_v1_to_v2(&mut connection).unwrap();

        assert_eq!(
            connection
                .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .unwrap(),
            2
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT event_type, content_digest IS NOT NULL, spec_body_digest IS NULL
                     FROM semantic_events",
                    [],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, bool>(1)?,
                            row.get::<_, bool>(2)?,
                        ))
                    },
                )
                .unwrap(),
            (1, true, true)
        );
        connection
            .execute(
                "INSERT INTO spec_bodies VALUES (?1, ?2)",
                rusqlite::params![[2_u8; 32].as_slice(), [3_u8; 32].as_slice()],
            )
            .unwrap();
        assert!(connection.execute("DELETE FROM spec_bodies", []).is_err());
    }
}
