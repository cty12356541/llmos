//! `SQLite` (`WAL` + `FULL`, `STRICT` tables, anti-rollback and immutability
//! triggers) storage schema and low-level codecs for `nlos-publisher-trust`
//! (`ADR-0027` section 2.3; `[PKG-IMM-001]`, `[PKG-APPID-001]`, `[PKG-SEP-001]`).

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, TransactionBehavior};

use crate::types::PublisherTrustCheckError;

/// Current durable schema version of `publisher-trust.db`.
pub const PUBLISHER_TRUST_SCHEMA_VERSION: i64 = 1;

/// Default database filename inside an authority directory root.
pub const PUBLISHER_TRUST_DB_FILENAME: &str = "publisher-trust.db";

/// Singleton primary key for `trust_revocation_state`.
pub(crate) const REVOCATION_STATE_SINGLETON_ID: i64 = 1;

const SCHEMA_V1_DDL: &str = "
CREATE TABLE IF NOT EXISTS trust_root_anchors (
    root_serial_id        BLOB PRIMARY KEY NOT NULL CHECK (length(root_serial_id) = 16),
    root_key_id           BLOB NOT NULL UNIQUE CHECK (length(root_key_id) = 16),
    root_public_key       BLOB NOT NULL UNIQUE CHECK (length(root_public_key) = 32),
    public_key_fingerprint BLOB NOT NULL UNIQUE CHECK (length(public_key_fingerprint) = 32),
    not_before_ms         INTEGER NOT NULL CHECK (not_before_ms >= 0),
    expires_at_ms         INTEGER NOT NULL CHECK (expires_at_ms > not_before_ms),
    self_signature        BLOB NOT NULL CHECK (length(self_signature) = 64),
    anchored_at_ms        INTEGER NOT NULL CHECK (anchored_at_ms >= 0)
) STRICT;

CREATE TRIGGER IF NOT EXISTS trust_root_anchors_no_update
BEFORE UPDATE ON trust_root_anchors
BEGIN
    SELECT RAISE(ABORT, 'trust_root_anchors rows are immutable');
END;

CREATE TRIGGER IF NOT EXISTS trust_root_anchors_no_delete
BEFORE DELETE ON trust_root_anchors
BEGIN
    SELECT RAISE(ABORT, 'trust_root_anchors rows cannot be deleted');
END;

CREATE TABLE IF NOT EXISTS store_intermediate_certificates (
    serial_id               BLOB PRIMARY KEY NOT NULL CHECK (length(serial_id) = 16),
    issuer_root_serial_id   BLOB NOT NULL CHECK (length(issuer_root_serial_id) = 16),
    intermediate_key_id     BLOB NOT NULL UNIQUE CHECK (length(intermediate_key_id) = 16),
    intermediate_public_key BLOB NOT NULL UNIQUE CHECK (length(intermediate_public_key) = 32),
    max_issuable_tier       INTEGER NOT NULL CHECK (max_issuable_tier IN (1, 2, 3)),
    not_before_ms           INTEGER NOT NULL CHECK (not_before_ms >= 0),
    expires_at_ms           INTEGER NOT NULL CHECK (expires_at_ms > not_before_ms),
    root_signature          BLOB NOT NULL CHECK (length(root_signature) = 64),
    registered_at_ms        INTEGER NOT NULL CHECK (registered_at_ms >= 0),
    FOREIGN KEY (issuer_root_serial_id) REFERENCES trust_root_anchors(root_serial_id)
) STRICT;

CREATE TRIGGER IF NOT EXISTS store_intermediate_certificates_no_update
BEFORE UPDATE ON store_intermediate_certificates
BEGIN
    SELECT RAISE(ABORT, 'store_intermediate_certificates rows are immutable');
END;

CREATE TRIGGER IF NOT EXISTS store_intermediate_certificates_no_delete
BEFORE DELETE ON store_intermediate_certificates
BEGIN
    SELECT RAISE(ABORT, 'store_intermediate_certificates rows cannot be deleted');
END;

CREATE TABLE IF NOT EXISTS publisher_leaf_certificates (
    serial_id              BLOB PRIMARY KEY NOT NULL CHECK (length(serial_id) = 16),
    publisher_id           BLOB NOT NULL CHECK (length(publisher_id) = 16),
    trust_tier             INTEGER NOT NULL CHECK (trust_tier IN (1, 2, 3)),
    leaf_key_id            BLOB NOT NULL UNIQUE CHECK (length(leaf_key_id) = 16),
    leaf_public_key        BLOB NOT NULL UNIQUE CHECK (length(leaf_public_key) = 32),
    not_before_ms          INTEGER NOT NULL CHECK (not_before_ms >= 0),
    expires_at_ms          INTEGER NOT NULL CHECK (expires_at_ms > not_before_ms),
    issuer_serial_id       BLOB NOT NULL CHECK (length(issuer_serial_id) = 16),
    signature              BLOB NOT NULL CHECK (length(signature) = 64),
    status                 INTEGER NOT NULL CHECK (status IN (1, 2, 3)),
    compromised_at_epoch   INTEGER NOT NULL CHECK (compromised_at_epoch >= 0),
    registered_at_ms       INTEGER NOT NULL CHECK (registered_at_ms >= 0)
) STRICT;

CREATE INDEX IF NOT EXISTS idx_publisher_leaf_by_publisher
ON publisher_leaf_certificates(publisher_id, leaf_public_key);

CREATE TRIGGER IF NOT EXISTS publisher_leaf_certificates_monotonic_update
BEFORE UPDATE ON publisher_leaf_certificates
BEGIN
    SELECT CASE
        WHEN NEW.serial_id != OLD.serial_id
          OR NEW.publisher_id != OLD.publisher_id
          OR NEW.trust_tier != OLD.trust_tier
          OR NEW.leaf_key_id != OLD.leaf_key_id
          OR NEW.leaf_public_key != OLD.leaf_public_key
          OR NEW.not_before_ms != OLD.not_before_ms
          OR NEW.expires_at_ms != OLD.expires_at_ms
          OR NEW.issuer_serial_id != OLD.issuer_serial_id
          OR NEW.signature != OLD.signature
          OR NEW.registered_at_ms != OLD.registered_at_ms
        THEN RAISE(ABORT, 'publisher_leaf_certificates identity columns are immutable')
        WHEN OLD.status = 3 AND NEW.status != 3
        THEN RAISE(ABORT, 'compromised publisher_leaf_certificates cannot be reactivated')
        WHEN NEW.status < OLD.status
        THEN RAISE(ABORT, 'publisher_leaf_certificates status transition must be monotonic')
    END;
END;

CREATE TRIGGER IF NOT EXISTS publisher_leaf_certificates_no_delete
BEFORE DELETE ON publisher_leaf_certificates
BEGIN
    SELECT RAISE(ABORT, 'publisher_leaf_certificates rows cannot be deleted');
END;

CREATE TABLE IF NOT EXISTS publisher_key_rotations (
    receipt_id         BLOB PRIMARY KEY NOT NULL CHECK (length(receipt_id) = 16),
    publisher_id       BLOB NOT NULL CHECK (length(publisher_id) = 16),
    old_leaf_serial_id BLOB NOT NULL CHECK (length(old_leaf_serial_id) = 16),
    old_leaf_key_id    BLOB NOT NULL CHECK (length(old_leaf_key_id) = 16),
    new_leaf_serial_id BLOB NOT NULL CHECK (length(new_leaf_serial_id) = 16),
    new_leaf_key_id    BLOB NOT NULL CHECK (length(new_leaf_key_id) = 16),
    disposition_code   INTEGER NOT NULL CHECK (disposition_code IN (1, 2)),
    revoked_at_epoch   INTEGER NOT NULL CHECK (revoked_at_epoch >= 0),
    proof_kind         INTEGER NOT NULL CHECK (proof_kind IN (1, 2)),
    proof_signer_id    BLOB NOT NULL CHECK (length(proof_signer_id) = 16),
    primary_signature  BLOB NOT NULL CHECK (length(primary_signature) = 64),
    new_leaf_signature BLOB NOT NULL CHECK (length(new_leaf_signature) = 64),
    rotated_at_ms      INTEGER NOT NULL CHECK (rotated_at_ms >= 0)
) STRICT;

CREATE TRIGGER IF NOT EXISTS publisher_key_rotations_no_update
BEFORE UPDATE ON publisher_key_rotations
BEGIN
    SELECT RAISE(ABORT, 'publisher_key_rotations rows are immutable');
END;

CREATE TRIGGER IF NOT EXISTS publisher_key_rotations_no_delete
BEFORE DELETE ON publisher_key_rotations
BEGIN
    SELECT RAISE(ABORT, 'publisher_key_rotations rows cannot be deleted');
END;

CREATE TABLE IF NOT EXISTS trust_revocation_state (
    singleton_id     INTEGER PRIMARY KEY NOT NULL CHECK (singleton_id = 1),
    revocation_epoch INTEGER NOT NULL CHECK (revocation_epoch >= 0),
    issued_at_ms     INTEGER NOT NULL CHECK (issued_at_ms >= 0),
    fresh_until_ms   INTEGER NOT NULL CHECK (fresh_until_ms >= issued_at_ms),
    crl_digest       BLOB NOT NULL CHECK (length(crl_digest) = 32)
) STRICT;

CREATE TRIGGER IF NOT EXISTS trust_revocation_state_no_rollback
BEFORE UPDATE ON trust_revocation_state
BEGIN
    SELECT CASE
        WHEN NEW.singleton_id != OLD.singleton_id
        THEN RAISE(ABORT, 'trust_revocation_state singleton_id is immutable')
        WHEN NEW.revocation_epoch < OLD.revocation_epoch
        THEN RAISE(ABORT, 'revocation_epoch rollback is forbidden')
        WHEN NEW.revocation_epoch = OLD.revocation_epoch AND NEW.crl_digest != OLD.crl_digest
        THEN RAISE(ABORT, 'revocation_epoch equivocation is forbidden')
    END;
END;

CREATE TRIGGER IF NOT EXISTS trust_revocation_state_no_delete
BEFORE DELETE ON trust_revocation_state
BEGIN
    SELECT RAISE(ABORT, 'trust_revocation_state cannot be deleted');
END;

CREATE TABLE IF NOT EXISTS revoked_certificates (
    serial_or_key    BLOB PRIMARY KEY NOT NULL CHECK (length(serial_or_key) = 16),
    revoked_at_epoch INTEGER NOT NULL CHECK (revoked_at_epoch >= 1),
    reason_code      INTEGER NOT NULL CHECK (reason_code IN (1, 2, 3, 4, 5)),
    recorded_at_ms   INTEGER NOT NULL CHECK (recorded_at_ms >= 0)
) STRICT;

CREATE TRIGGER IF NOT EXISTS revoked_certificates_no_update
BEFORE UPDATE ON revoked_certificates
BEGIN
    SELECT RAISE(ABORT, 'revoked_certificates rows are immutable');
END;

CREATE TRIGGER IF NOT EXISTS revoked_certificates_no_delete
BEFORE DELETE ON revoked_certificates
BEGIN
    SELECT RAISE(ABORT, 'revoked_certificates rows cannot be deleted');
END;

CREATE TABLE IF NOT EXISTS revoked_publishers (
    publisher_id     BLOB PRIMARY KEY NOT NULL CHECK (length(publisher_id) = 16),
    revoked_at_epoch INTEGER NOT NULL CHECK (revoked_at_epoch >= 1),
    recorded_at_ms   INTEGER NOT NULL CHECK (recorded_at_ms >= 0)
) STRICT;

CREATE TRIGGER IF NOT EXISTS revoked_publishers_no_update
BEFORE UPDATE ON revoked_publishers
BEGIN
    SELECT RAISE(ABORT, 'revoked_publishers rows are immutable');
END;

CREATE TRIGGER IF NOT EXISTS revoked_publishers_no_delete
BEFORE DELETE ON revoked_publishers
BEGIN
    SELECT RAISE(ABORT, 'revoked_publishers rows cannot be deleted');
END;

CREATE TABLE IF NOT EXISTS application_ownership_transfers (
    receipt_id             BLOB PRIMARY KEY NOT NULL CHECK (length(receipt_id) = 16),
    application_id         BLOB NOT NULL CHECK (length(application_id) = 16),
    prior_generation       INTEGER NOT NULL CHECK (prior_generation >= 1),
    new_generation         INTEGER NOT NULL CHECK (new_generation = prior_generation + 1),
    from_publisher_id      BLOB NOT NULL CHECK (length(from_publisher_id) = 16),
    from_leaf_public_key   BLOB NOT NULL CHECK (length(from_leaf_public_key) = 32),
    to_publisher_id        BLOB NOT NULL CHECK (length(to_publisher_id) = 16 AND to_publisher_id != from_publisher_id),
    to_trust_tier          INTEGER NOT NULL CHECK (to_trust_tier IN (1, 2, 3)),
    to_leaf_public_key     BLOB NOT NULL CHECK (length(to_leaf_public_key) = 32),
    intermediate_serial_id BLOB NOT NULL CHECK (length(intermediate_serial_id) = 16),
    from_owner_signature   BLOB NOT NULL CHECK (length(from_owner_signature) = 64),
    to_owner_signature     BLOB NOT NULL CHECK (length(to_owner_signature) = 64),
    store_ca_signature     BLOB NOT NULL CHECK (length(store_ca_signature) = 64),
    transferred_at_ms      INTEGER NOT NULL CHECK (transferred_at_ms >= 0),
    UNIQUE (application_id, new_generation)
) STRICT;

CREATE TRIGGER IF NOT EXISTS application_ownership_transfers_no_update
BEFORE UPDATE ON application_ownership_transfers
BEGIN
    SELECT RAISE(ABORT, 'application_ownership_transfers rows are immutable');
END;

CREATE TRIGGER IF NOT EXISTS application_ownership_transfers_no_delete
BEFORE DELETE ON application_ownership_transfers
BEGIN
    SELECT RAISE(ABORT, 'application_ownership_transfers rows cannot be deleted');
END;

CREATE TABLE IF NOT EXISTS application_ownership_bindings (
    application_id          BLOB PRIMARY KEY NOT NULL CHECK (length(application_id) = 16),
    owner_publisher_id      BLOB NOT NULL CHECK (length(owner_publisher_id) = 16),
    trust_tier              INTEGER NOT NULL CHECK (trust_tier IN (1, 2, 3)),
    first_registered_at_ms  INTEGER NOT NULL CHECK (first_registered_at_ms >= 0),
    updated_at_ms           INTEGER NOT NULL CHECK (updated_at_ms >= first_registered_at_ms),
    registration_generation INTEGER NOT NULL CHECK (registration_generation >= 1)
) STRICT;

CREATE TRIGGER IF NOT EXISTS application_ownership_bindings_transfer_gated
BEFORE UPDATE ON application_ownership_bindings
BEGIN
    SELECT CASE
        WHEN NEW.application_id != OLD.application_id
          OR NEW.first_registered_at_ms != OLD.first_registered_at_ms
        THEN RAISE(ABORT, 'application_ownership_bindings immutable columns cannot change')
        WHEN NEW.registration_generation != OLD.registration_generation + 1
        THEN RAISE(ABORT, 'application_ownership_bindings generation must advance by exactly 1')
        WHEN NOT EXISTS (
            SELECT 1 FROM application_ownership_transfers t
            WHERE t.application_id = OLD.application_id
              AND t.prior_generation = OLD.registration_generation
              AND t.new_generation = NEW.registration_generation
              AND t.from_publisher_id = OLD.owner_publisher_id
              AND t.to_publisher_id = NEW.owner_publisher_id
              AND t.to_trust_tier = NEW.trust_tier
        )
        THEN RAISE(ABORT, 'application_ownership_bindings update requires a verified 3-party transfer receipt')
    END;
END;

CREATE TRIGGER IF NOT EXISTS application_ownership_bindings_no_delete
BEFORE DELETE ON application_ownership_bindings
BEGIN
    SELECT RAISE(ABORT, 'application_ownership_bindings rows cannot be deleted');
END;

CREATE TABLE IF NOT EXISTS package_release_bindings (
    package_id       BLOB PRIMARY KEY NOT NULL CHECK (length(package_id) = 16),
    application_id   BLOB NOT NULL CHECK (length(application_id) = 16 AND package_id != application_id),
    publisher_id     BLOB NOT NULL CHECK (length(publisher_id) = 16),
    manifest_digest  BLOB NOT NULL CHECK (length(manifest_digest) = 32),
    registered_at_ms INTEGER NOT NULL CHECK (registered_at_ms >= 0)
) STRICT;

CREATE INDEX IF NOT EXISTS idx_package_release_by_app
ON package_release_bindings(application_id, publisher_id);

CREATE TRIGGER IF NOT EXISTS package_release_bindings_no_update
BEFORE UPDATE ON package_release_bindings
BEGIN
    SELECT RAISE(ABORT, 'package_release_bindings rows are immutable (PKG-IMM-001)');
END;

CREATE TRIGGER IF NOT EXISTS package_release_bindings_no_delete
BEFORE DELETE ON package_release_bindings
BEGIN
    SELECT RAISE(ABORT, 'package_release_bindings rows cannot be deleted (PKG-IMM-001)');
END;
";

pub(crate) fn open_verified_connection(
    path: &Path,
) -> Result<Connection, PublisherTrustCheckError> {
    let db_path = if path.extension().is_some() {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|err| {
                PublisherTrustCheckError::StorageFailure(format!(
                    "create parent directory failed: {err}"
                ))
            })?;
        }
        path.to_path_buf()
    } else {
        std::fs::create_dir_all(path).map_err(|err| {
            PublisherTrustCheckError::StorageFailure(format!(
                "create authority directory failed: {err}"
            ))
        })?;
        path.join(PUBLISHER_TRUST_DB_FILENAME)
    };

    let mut connection = Connection::open(db_path).map_err(sql_err)?;
    connection
        .busy_timeout(Duration::from_secs(5))
        .map_err(sql_err)?;
    connection
        .pragma_update(None, "journal_mode", "WAL")
        .map_err(sql_err)?;
    connection
        .pragma_update(None, "synchronous", "FULL")
        .map_err(sql_err)?;
    connection
        .pragma_update(None, "foreign_keys", "ON")
        .map_err(sql_err)?;

    let journal_mode: String = connection
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .map_err(sql_err)?;
    let synchronous: i64 = connection
        .pragma_query_value(None, "synchronous", |row| row.get(0))
        .map_err(sql_err)?;
    if !journal_mode.eq_ignore_ascii_case("wal") || synchronous != 2 {
        return Err(PublisherTrustCheckError::StorageFailure(format!(
            "SQLite WAL+FULL durability unavailable: journal_mode={journal_mode}, synchronous={synchronous}"
        )));
    }

    init_or_validate_schema(&mut connection)?;
    Ok(connection)
}

fn init_or_validate_schema(connection: &mut Connection) -> Result<(), PublisherTrustCheckError> {
    let tx = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_err)?;
    let user_version: i64 = tx
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(sql_err)?;

    match user_version {
        0 => {
            tx.execute_batch(SCHEMA_V1_DDL).map_err(sql_err)?;
            tx.execute(
                "INSERT OR IGNORE INTO trust_revocation_state (
                    singleton_id, revocation_epoch, issued_at_ms, fresh_until_ms, crl_digest
                 ) VALUES (?1, 0, 0, 0, ?2)",
                rusqlite::params![REVOCATION_STATE_SINGLETON_ID, [0_u8; 32].as_slice()],
            )
            .map_err(sql_err)?;
            tx.pragma_update(None, "user_version", PUBLISHER_TRUST_SCHEMA_VERSION)
                .map_err(sql_err)?;
        }
        PUBLISHER_TRUST_SCHEMA_VERSION => {
            tx.execute_batch(SCHEMA_V1_DDL).map_err(sql_err)?;
        }
        other => {
            return Err(PublisherTrustCheckError::StorageFailure(format!(
                "unsupported publisher trust schema user_version={other} (expected {PUBLISHER_TRUST_SCHEMA_VERSION})"
            )));
        }
    }
    tx.commit().map_err(sql_err)?;
    Ok(())
}

#[allow(clippy::needless_pass_by_value)] // Passed directly to Result::map_err(sql_err).
pub(crate) fn sql_err(err: rusqlite::Error) -> PublisherTrustCheckError {
    PublisherTrustCheckError::StorageFailure(err.to_string())
}

pub(crate) fn encode_u64(value: u64) -> Result<i64, PublisherTrustCheckError> {
    i64::try_from(value).map_err(|_| {
        PublisherTrustCheckError::StorageFailure(format!(
            "u64 value {value} exceeds SQLite signed 64-bit integer range"
        ))
    })
}

pub(crate) fn decode_u64(value: i64, field: &'static str) -> Result<u64, PublisherTrustCheckError> {
    u64::try_from(value).map_err(|_| {
        PublisherTrustCheckError::StorageFailure(format!(
            "corrupt negative integer {value} in column {field}"
        ))
    })
}

pub(crate) fn fixed16(
    bytes: Vec<u8>,
    field: &'static str,
) -> Result<[u8; 16], PublisherTrustCheckError> {
    bytes.try_into().map_err(|_| {
        PublisherTrustCheckError::StorageFailure(format!(
            "corrupt 16-byte column width for {field}"
        ))
    })
}

pub(crate) fn fixed32(
    bytes: Vec<u8>,
    field: &'static str,
) -> Result<[u8; 32], PublisherTrustCheckError> {
    bytes.try_into().map_err(|_| {
        PublisherTrustCheckError::StorageFailure(format!(
            "corrupt 32-byte column width for {field}"
        ))
    })
}

pub(crate) fn fixed64(
    bytes: Vec<u8>,
    field: &'static str,
) -> Result<[u8; 64], PublisherTrustCheckError> {
    bytes.try_into().map_err(|_| {
        PublisherTrustCheckError::StorageFailure(format!(
            "corrupt 64-byte column width for {field}"
        ))
    })
}
