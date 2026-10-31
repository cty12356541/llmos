//! `SQLite` `STRICT` schema and immutability / monotonic-epoch DDL triggers for
//! `nlos-store-registry` (`WAL + FULL`, `PRAGMA user_version = 1`).

use rusqlite::{Connection, TransactionBehavior};

use crate::error::StoreRegistryError;

pub(crate) const SCHEMA_VERSION: i64 = 1;

#[allow(clippy::too_many_lines)]
pub(crate) fn migrate_v1(connection: &mut Connection) -> Result<(), StoreRegistryError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE TABLE store_governance_meta (
            singleton_id INTEGER PRIMARY KEY CHECK (singleton_id = 1),
            store_revocation_epoch INTEGER NOT NULL CHECK (store_revocation_epoch >= 0),
            certifier_public_key BLOB NOT NULL CHECK (length(certifier_public_key) = 32),
            entitlement_issuer_key_id BLOB NOT NULL CHECK (length(entitlement_issuer_key_id) = 16),
            entitlement_issuer_public_key BLOB NOT NULL CHECK (length(entitlement_issuer_public_key) = 32),
            recall_authority_public_key BLOB NOT NULL CHECK (length(recall_authority_public_key) = 32),
            updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= 0)
        ) STRICT;

        CREATE TRIGGER trg_store_meta_no_delete
        BEFORE DELETE ON store_governance_meta
        BEGIN
            SELECT RAISE(ABORT, 'store_governance_meta cannot be deleted');
        END;

        CREATE TRIGGER trg_store_meta_monotonic_epoch
        BEFORE UPDATE ON store_governance_meta
        WHEN NEW.singleton_id != OLD.singleton_id
          OR NEW.store_revocation_epoch < OLD.store_revocation_epoch
          OR NEW.certifier_public_key != OLD.certifier_public_key
          OR NEW.entitlement_issuer_key_id != OLD.entitlement_issuer_key_id
          OR NEW.entitlement_issuer_public_key != OLD.entitlement_issuer_public_key
          OR NEW.recall_authority_public_key != OLD.recall_authority_public_key
        BEGIN
            SELECT RAISE(ABORT, 'store_revocation_epoch regression or trust anchor mutation forbidden');
        END;

        CREATE TABLE store_application_owners (
            application_id BLOB PRIMARY KEY CHECK (length(application_id) = 16),
            owner_publisher_id BLOB NOT NULL CHECK (length(owner_publisher_id) = 16),
            trust_tier INTEGER NOT NULL CHECK (trust_tier IN (1, 2, 3)),
            first_registered_at_ms INTEGER NOT NULL CHECK (first_registered_at_ms >= 0),
            registration_generation INTEGER NOT NULL CHECK (registration_generation >= 1)
        ) STRICT;

        CREATE TRIGGER trg_store_app_owners_no_update
        BEFORE UPDATE ON store_application_owners
        BEGIN
            SELECT RAISE(ABORT, 'store_application_owners binding is immutable (PKG-APPID-001)');
        END;

        CREATE TRIGGER trg_store_app_owners_no_delete
        BEFORE DELETE ON store_application_owners
        BEGIN
            SELECT RAISE(ABORT, 'store_application_owners binding cannot be deleted');
        END;

        CREATE TABLE store_releases (
            package_id BLOB NOT NULL CHECK (length(package_id) = 16),
            package_version INTEGER NOT NULL CHECK (package_version >= 0),
            manifest_digest BLOB NOT NULL CHECK (length(manifest_digest) = 32),
            oci_manifest_digest BLOB NOT NULL CHECK (length(oci_manifest_digest) = 32),
            publisher_id BLOB NOT NULL CHECK (length(publisher_id) = 16),
            publisher_trust_tier INTEGER NOT NULL CHECK (publisher_trust_tier IN (1, 2, 3)),
            publisher_signing_key BLOB NOT NULL CHECK (length(publisher_signing_key) = 32),
            publisher_signature BLOB NOT NULL CHECK (length(publisher_signature) = 64),
            channel INTEGER NOT NULL CHECK (channel IN (1, 2, 3)),
            track INTEGER NOT NULL CHECK (track IN (1, 2)),
            active_recall_severity INTEGER CHECK (
                active_recall_severity IS NULL OR active_recall_severity IN (1, 2, 3)
            ),
            active_recall_reason TEXT,
            submitted_at_ms INTEGER NOT NULL CHECK (submitted_at_ms >= 0),
            updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= 0),
            PRIMARY KEY (package_id, package_version),
            CHECK (NOT (publisher_trust_tier = 3 AND channel = 2)),
            CHECK (NOT (track = 2 AND channel = 2)),
            CHECK (
                (publisher_trust_tier = 3 AND track = 2)
                OR (publisher_trust_tier IN (1, 2) AND track = 1)
            )
        ) STRICT;

        CREATE TRIGGER trg_store_releases_no_delete
        BEFORE DELETE ON store_releases
        BEGIN
            SELECT RAISE(ABORT, 'store_releases rows are append-only');
        END;

        CREATE TRIGGER trg_store_releases_state_guard
        BEFORE UPDATE ON store_releases
        WHEN NEW.package_id != OLD.package_id
          OR NEW.package_version != OLD.package_version
          OR NEW.manifest_digest != OLD.manifest_digest
          OR NEW.oci_manifest_digest != OLD.oci_manifest_digest
          OR NEW.publisher_id != OLD.publisher_id
          OR NEW.publisher_trust_tier != OLD.publisher_trust_tier
          OR NEW.publisher_signing_key != OLD.publisher_signing_key
          OR NEW.publisher_signature != OLD.publisher_signature
          OR NEW.track != OLD.track
          OR NEW.submitted_at_ms != OLD.submitted_at_ms
          OR (OLD.channel = 3 AND NEW.channel != 3)
          OR (OLD.active_recall_severity IS NOT NULL AND NEW.active_recall_severity IS NULL)
          OR (
              OLD.active_recall_severity IS NOT NULL
              AND NEW.active_recall_severity < OLD.active_recall_severity
          )
        BEGIN
            SELECT RAISE(ABORT, 'store_releases immutable identity or recall anti-downgrade violation');
        END;

        CREATE TABLE store_release_applications (
            package_id BLOB NOT NULL CHECK (length(package_id) = 16),
            package_version INTEGER NOT NULL CHECK (package_version >= 0),
            app_ordinal INTEGER NOT NULL CHECK (app_ordinal >= 0),
            application_id BLOB NOT NULL CHECK (length(application_id) = 16),
            PRIMARY KEY (package_id, package_version, application_id),
            FOREIGN KEY (package_id, package_version)
                REFERENCES store_releases(package_id, package_version)
        ) STRICT;

        CREATE TRIGGER trg_store_release_apps_no_update
        BEFORE UPDATE ON store_release_applications
        BEGIN
            SELECT RAISE(ABORT, 'store_release_applications rows are immutable');
        END;

        CREATE TRIGGER trg_store_release_apps_no_delete
        BEFORE DELETE ON store_release_applications
        BEGIN
            SELECT RAISE(ABORT, 'store_release_applications rows cannot be deleted');
        END;

        CREATE TABLE store_release_components (
            package_id BLOB NOT NULL CHECK (length(package_id) = 16),
            package_version INTEGER NOT NULL CHECK (package_version >= 0),
            component_ordinal INTEGER NOT NULL CHECK (component_ordinal >= 0),
            slot_name TEXT NOT NULL CHECK (length(slot_name) > 0),
            component_kind INTEGER NOT NULL CHECK (component_kind IN (1, 2, 3, 4, 5)),
            entrypoint TEXT NOT NULL CHECK (length(entrypoint) > 0),
            content_sha256 BLOB NOT NULL CHECK (length(content_sha256) = 32),
            size_bytes INTEGER NOT NULL CHECK (size_bytes >= 0),
            PRIMARY KEY (package_id, package_version, slot_name),
            FOREIGN KEY (package_id, package_version)
                REFERENCES store_releases(package_id, package_version)
        ) STRICT;

        CREATE TRIGGER trg_store_release_components_no_update
        BEFORE UPDATE ON store_release_components
        BEGIN
            SELECT RAISE(ABORT, 'store_release_components rows are immutable');
        END;

        CREATE TRIGGER trg_store_release_components_no_delete
        BEFORE DELETE ON store_release_components
        BEGIN
            SELECT RAISE(ABORT, 'store_release_components rows cannot be deleted');
        END;

        CREATE TABLE store_permission_transparency (
            package_id BLOB NOT NULL CHECK (length(package_id) = 16),
            package_version INTEGER NOT NULL CHECK (package_version >= 0),
            perm_ordinal INTEGER NOT NULL CHECK (perm_ordinal >= 0),
            capability_scope TEXT NOT NULL CHECK (length(capability_scope) > 0),
            human_purpose TEXT NOT NULL CHECK (length(human_purpose) > 0),
            effect_class INTEGER NOT NULL CHECK (effect_class IN (1, 2, 3, 4)),
            network_egress_declared INTEGER NOT NULL CHECK (network_egress_declared IN (0, 1)),
            sandbox_profile TEXT NOT NULL CHECK (length(sandbox_profile) > 0),
            PRIMARY KEY (package_id, package_version, capability_scope),
            FOREIGN KEY (package_id, package_version)
                REFERENCES store_releases(package_id, package_version)
        ) STRICT;

        CREATE TRIGGER trg_store_perm_transparency_no_update
        BEFORE UPDATE ON store_permission_transparency
        BEGIN
            SELECT RAISE(ABORT, 'store_permission_transparency rows are immutable');
        END;

        CREATE TRIGGER trg_store_perm_transparency_no_delete
        BEFORE DELETE ON store_permission_transparency
        BEGIN
            SELECT RAISE(ABORT, 'store_permission_transparency rows cannot be deleted');
        END;

        CREATE TABLE store_certification_receipts (
            idempotency_key BLOB PRIMARY KEY CHECK (length(idempotency_key) = 16),
            request_digest BLOB NOT NULL CHECK (length(request_digest) = 32),
            receipt_id BLOB NOT NULL UNIQUE CHECK (length(receipt_id) = 16),
            package_id BLOB NOT NULL CHECK (length(package_id) = 16),
            package_version INTEGER NOT NULL CHECK (package_version >= 0),
            application_id BLOB NOT NULL CHECK (length(application_id) = 16),
            manifest_digest BLOB NOT NULL CHECK (length(manifest_digest) = 32),
            oci_manifest_digest BLOB NOT NULL CHECK (length(oci_manifest_digest) = 32),
            publisher_id BLOB NOT NULL CHECK (length(publisher_id) = 16),
            passed_gates_mask INTEGER NOT NULL CHECK (passed_gates_mask = 15),
            issued_at_ms INTEGER NOT NULL CHECK (issued_at_ms >= 0),
            expires_at_ms INTEGER NOT NULL CHECK (expires_at_ms >= issued_at_ms),
            certifier_public_key BLOB NOT NULL CHECK (length(certifier_public_key) = 32),
            ed25519_signature BLOB NOT NULL CHECK (length(ed25519_signature) = 64),
            promoted_at_ms INTEGER NOT NULL CHECK (promoted_at_ms >= 0)
        ) STRICT;

        CREATE TRIGGER trg_store_cert_receipts_no_update
        BEFORE UPDATE ON store_certification_receipts
        BEGIN
            SELECT RAISE(ABORT, 'store_certification_receipts are immutable');
        END;

        CREATE TRIGGER trg_store_cert_receipts_no_delete
        BEFORE DELETE ON store_certification_receipts
        BEGIN
            SELECT RAISE(ABORT, 'store_certification_receipts cannot be deleted');
        END;

        CREATE TABLE store_entitlement_usage (
            token_id BLOB PRIMARY KEY CHECK (length(token_id) = 16),
            holder_principal BLOB NOT NULL CHECK (length(holder_principal) = 16),
            application_id BLOB NOT NULL CHECK (length(application_id) = 16),
            max_seat_count INTEGER NOT NULL CHECK (max_seat_count >= 1),
            redeemed_seat_count INTEGER NOT NULL CHECK (
                redeemed_seat_count >= 1 AND redeemed_seat_count <= max_seat_count
            ),
            updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= 0)
        ) STRICT;

        CREATE TRIGGER trg_store_entitlement_usage_monotonic
        BEFORE UPDATE ON store_entitlement_usage
        WHEN NEW.token_id != OLD.token_id
          OR NEW.holder_principal != OLD.holder_principal
          OR NEW.application_id != OLD.application_id
          OR NEW.max_seat_count != OLD.max_seat_count
          OR NEW.redeemed_seat_count < OLD.redeemed_seat_count
        BEGIN
            SELECT RAISE(ABORT, 'store_entitlement_usage seat counter regression forbidden');
        END;

        CREATE TRIGGER trg_store_entitlement_usage_no_delete
        BEFORE DELETE ON store_entitlement_usage
        BEGIN
            SELECT RAISE(ABORT, 'store_entitlement_usage cannot be deleted');
        END;

        CREATE TABLE store_entitlement_receipts (
            idempotency_key BLOB PRIMARY KEY CHECK (length(idempotency_key) = 16),
            request_digest BLOB NOT NULL CHECK (length(request_digest) = 32),
            receipt_id BLOB NOT NULL UNIQUE CHECK (length(receipt_id) = 16),
            token_id BLOB NOT NULL CHECK (length(token_id) = 16),
            holder_principal BLOB NOT NULL CHECK (length(holder_principal) = 16),
            application_id BLOB NOT NULL CHECK (length(application_id) = 16),
            package_id BLOB NOT NULL CHECK (length(package_id) = 16),
            package_version INTEGER NOT NULL CHECK (package_version >= 0),
            granted_channel INTEGER NOT NULL CHECK (granted_channel IN (1, 2)),
            seat_ordinal INTEGER NOT NULL CHECK (seat_ordinal >= 1),
            remaining_seats INTEGER NOT NULL CHECK (remaining_seats >= 0),
            store_revocation_epoch INTEGER NOT NULL CHECK (store_revocation_epoch >= 0),
            redeemed_at_ms INTEGER NOT NULL CHECK (redeemed_at_ms >= 0)
        ) STRICT;

        CREATE TRIGGER trg_store_entitlement_receipts_no_update
        BEFORE UPDATE ON store_entitlement_receipts
        BEGIN
            SELECT RAISE(ABORT, 'store_entitlement_receipts are immutable');
        END;

        CREATE TRIGGER trg_store_entitlement_receipts_no_delete
        BEFORE DELETE ON store_entitlement_receipts
        BEGIN
            SELECT RAISE(ABORT, 'store_entitlement_receipts cannot be deleted');
        END;

        CREATE TABLE store_granted_capabilities (
            capability_id BLOB PRIMARY KEY CHECK (length(capability_id) = 16),
            issued_generation INTEGER NOT NULL CHECK (issued_generation >= 1),
            package_id BLOB NOT NULL CHECK (length(package_id) = 16),
            package_version INTEGER NOT NULL CHECK (package_version >= 0),
            application_id BLOB NOT NULL CHECK (length(application_id) = 16),
            installation_id BLOB CHECK (installation_id IS NULL OR length(installation_id) = 16),
            registered_at_ms INTEGER NOT NULL CHECK (registered_at_ms >= 0),
            revoked_by_advisory_id BLOB CHECK (
                revoked_by_advisory_id IS NULL OR length(revoked_by_advisory_id) = 16
            ),
            revoked_resulting_generation INTEGER CHECK (
                revoked_resulting_generation IS NULL OR revoked_resulting_generation >= 2
            ),
            revoked_at_ms INTEGER CHECK (revoked_at_ms IS NULL OR revoked_at_ms >= 0)
        ) STRICT;

        CREATE TRIGGER trg_store_granted_caps_guard
        BEFORE UPDATE ON store_granted_capabilities
        WHEN NEW.capability_id != OLD.capability_id
          OR NEW.issued_generation != OLD.issued_generation
          OR NEW.package_id != OLD.package_id
          OR NEW.package_version != OLD.package_version
          OR NEW.application_id != OLD.application_id
          OR NEW.installation_id IS NOT OLD.installation_id
          OR NEW.registered_at_ms != OLD.registered_at_ms
          OR (OLD.revoked_by_advisory_id IS NOT NULL AND NEW.revoked_by_advisory_id != OLD.revoked_by_advisory_id)
        BEGIN
            SELECT RAISE(ABORT, 'store_granted_capabilities revocation record is immutable once set');
        END;

        CREATE TRIGGER trg_store_granted_caps_no_delete
        BEFORE DELETE ON store_granted_capabilities
        BEGIN
            SELECT RAISE(ABORT, 'store_granted_capabilities rows cannot be deleted');
        END;

        CREATE TABLE store_recall_advisories (
            idempotency_key BLOB PRIMARY KEY CHECK (length(idempotency_key) = 16),
            request_digest BLOB NOT NULL CHECK (length(request_digest) = 32),
            receipt_id BLOB NOT NULL UNIQUE CHECK (length(receipt_id) = 16),
            advisory_id BLOB NOT NULL UNIQUE CHECK (length(advisory_id) = 16),
            package_id BLOB NOT NULL CHECK (length(package_id) = 16),
            application_id BLOB NOT NULL CHECK (length(application_id) = 16),
            target_version INTEGER CHECK (target_version IS NULL OR target_version >= 0),
            severity INTEGER NOT NULL CHECK (severity IN (1, 2, 3)),
            revocation_epoch INTEGER NOT NULL CHECK (revocation_epoch >= 1),
            reason TEXT NOT NULL,
            issued_at_ms INTEGER NOT NULL CHECK (issued_at_ms >= 0),
            applied_at_ms INTEGER NOT NULL CHECK (applied_at_ms >= 0),
            authority_signature BLOB NOT NULL CHECK (length(authority_signature) = 64),
            revoked_capability_count INTEGER NOT NULL CHECK (revoked_capability_count >= 0),
            application_quarantined INTEGER NOT NULL CHECK (application_quarantined IN (0, 1))
        ) STRICT;

        CREATE TRIGGER trg_store_recall_advisories_no_update
        BEFORE UPDATE ON store_recall_advisories
        BEGIN
            SELECT RAISE(ABORT, 'store_recall_advisories receipts are immutable');
        END;

        CREATE TRIGGER trg_store_recall_advisories_no_delete
        BEFORE DELETE ON store_recall_advisories
        BEGIN
            SELECT RAISE(ABORT, 'store_recall_advisories receipts cannot be deleted');
        END;",
    )?;
    transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    transaction.commit()?;
    Ok(())
}
