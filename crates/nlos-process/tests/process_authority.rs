use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use nlos_process::{
    ActiveProcessBinding, CreateIsolationDomainRequest, IsolationDomainDecision,
    IsolationDomainRotationDecision, ProcessAuthority, ProcessAuthorityError,
    ProcessBindingDecision, RegisterDelegatedProcessRequest, RestoreProcessDecision,
    RestoreProcessRequest, RotateIsolationDomainRequest,
};
use nlos_types::{Generation, IdempotencyKey, TaskAttemptId, TaskId};
use rusqlite::Connection;

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

struct TestRoot(PathBuf);

impl TestRoot {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let sequence = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "nlos-process-{label}-{}-{nonce}-{sequence}",
            std::process::id()
        ));
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn domain_request(seed: u8) -> CreateIsolationDomainRequest {
    CreateIsolationDomainRequest {
        policy_digest: [seed; 32],
        idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(1); 16]),
        created_at_ms: 1_000 + u64::from(seed),
    }
}

fn registration(
    seed: u8,
    domain: &nlos_process::IsolationDomainRecord,
) -> RegisterDelegatedProcessRequest {
    RegisterDelegatedProcessRequest {
        task_id: TaskId::from_bytes([seed.wrapping_add(2); 16]),
        task_attempt_id: TaskAttemptId::from_bytes([seed.wrapping_add(3); 16]),
        attempt_generation: Generation::INITIAL,
        isolation_domain_id: domain.isolation_domain_id,
        isolation_domain_generation: domain.generation,
        isolation_domain_fencing_token: domain.fencing_token,
        idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(6); 16]),
        created_at_ms: 2_000 + u64::from(seed),
    }
}

fn rotate_request(
    seed: u8,
    domain: &nlos_process::IsolationDomainRecord,
) -> RotateIsolationDomainRequest {
    RotateIsolationDomainRequest {
        isolation_domain_id: domain.isolation_domain_id,
        expected_generation: domain.generation,
        expected_fencing_token: domain.fencing_token,
        idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(7); 16]),
        rotated_at_ms: 3_000 + u64::from(seed),
    }
}

#[test]
fn authority_assigns_identities_and_replays_across_restart() {
    let root = TestRoot::new("register-restart");
    let request = domain_request(10);
    let (domain, binding) = {
        let authority = ProcessAuthority::open(root.path()).expect("open");
        let first = authority
            .create_isolation_domain(request)
            .expect("create domain");
        assert!(matches!(first, IsolationDomainDecision::Created(_)));
        let domain = first.record().clone();
        let process_request = registration(10, &domain);
        let first = authority
            .register_delegated_process(process_request)
            .expect("register process");
        assert!(matches!(first, ProcessBindingDecision::Registered(_)));
        let replay = authority
            .register_delegated_process(process_request)
            .expect("replay process");
        assert!(matches!(replay, ProcessBindingDecision::Replayed(_)));
        assert_eq!(first.record(), replay.record());
        (domain, first.record().clone())
    };

    let reopened = ProcessAuthority::open(root.path()).expect("reopen");
    assert_eq!(
        reopened
            .create_isolation_domain(request)
            .expect("domain replay")
            .record(),
        &domain
    );
    assert_eq!(
        reopened
            .inspect_active_process_binding(binding.process_id)
            .expect("active binding"),
        binding
    );
}

#[test]
fn idempotency_rebinding_and_stale_domain_fail_closed() {
    let root = TestRoot::new("conflicts");
    let authority = ProcessAuthority::open(root.path()).expect("open");
    let domain = authority
        .create_isolation_domain(domain_request(20))
        .expect("domain")
        .record()
        .clone();
    let request = registration(20, &domain);
    authority
        .register_delegated_process(request)
        .expect("register");

    let mut rebound = request;
    rebound.task_id = TaskId::from_bytes([0xee; 16]);
    assert!(matches!(
        authority.register_delegated_process(rebound),
        Err(ProcessAuthorityError::IdempotencyConflict)
    ));

    let mut stale = registration(21, &domain);
    stale.isolation_domain_fencing_token = [0xff; 32];
    assert!(matches!(
        authority.register_delegated_process(stale),
        Err(ProcessAuthorityError::StaleIsolationDomain)
    ));
}

#[test]
fn domain_rotation_is_replayable_and_fences_existing_binding() {
    let root = TestRoot::new("domain-rotation");
    let authority = ProcessAuthority::open(root.path()).expect("open");
    let domain = authority
        .create_isolation_domain(domain_request(30))
        .expect("domain")
        .record()
        .clone();
    let binding = authority
        .register_delegated_process(registration(30, &domain))
        .expect("register")
        .record()
        .clone();
    let request = rotate_request(30, &domain);
    let rotated = authority.rotate_isolation_domain(request).expect("rotate");
    assert!(matches!(
        rotated,
        IsolationDomainRotationDecision::Rotated(_)
    ));
    assert_eq!(rotated.record().generation.get(), 2);
    assert_ne!(rotated.record().fencing_token, domain.fencing_token);
    assert!(matches!(
        authority.inspect_active_process_binding(binding.process_id),
        Err(ProcessAuthorityError::StaleIsolationDomain)
    ));

    drop(authority);
    let reopened = ProcessAuthority::open(root.path()).expect("reopen");
    let replay = reopened
        .rotate_isolation_domain(request)
        .expect("rotation replay");
    assert!(matches!(
        replay,
        IsolationDomainRotationDecision::Replayed(_)
    ));
    assert_eq!(replay.record(), rotated.record());
}

#[test]
fn restore_advances_process_and_agent_generations_and_fences_old_reference() {
    let root = TestRoot::new("restore");
    let (old, restored) = {
        let authority = ProcessAuthority::open(root.path()).expect("open");
        let domain = authority
            .create_isolation_domain(domain_request(40))
            .expect("domain")
            .record()
            .clone();
        let old = authority
            .register_delegated_process(registration(40, &domain))
            .expect("register")
            .record()
            .clone();
        let rotated = authority
            .rotate_isolation_domain(rotate_request(40, &domain))
            .expect("rotate")
            .record()
            .clone();
        let restore = RestoreProcessRequest {
            process_id: old.process_id,
            expected_process_generation: old.process_generation,
            expected_process_fencing_token: old.process_fencing_token,
            isolation_domain_id: rotated.isolation_domain_id,
            isolation_domain_generation: rotated.generation,
            isolation_domain_fencing_token: rotated.fencing_token,
            idempotency_key: IdempotencyKey::from_bytes([0xc0; 16]),
            restored_at_ms: 4_000,
        };
        let restored = authority.restore_process(restore).expect("restore");
        assert!(matches!(restored, RestoreProcessDecision::Restored(_)));
        assert_eq!(restored.record().process_generation.get(), 2);
        assert_eq!(restored.record().agent_instance_generation.get(), 2);
        assert_eq!(restored.record().agent_instance_id, old.agent_instance_id);
        assert!(matches!(
            authority.verify_active_process_binding(&ActiveProcessBinding::from(&old)),
            Err(ProcessAuthorityError::StaleProcessBinding)
        ));
        assert_eq!(
            authority
                .verify_active_process_binding(&ActiveProcessBinding::from(restored.record()))
                .expect("verify current"),
            *restored.record()
        );
        let replay = authority.restore_process(restore).expect("restore replay");
        assert!(matches!(replay, RestoreProcessDecision::Replayed(_)));
        assert_eq!(replay.record(), restored.record());
        (old, restored.record().clone())
    };

    let reopened = ProcessAuthority::open(root.path()).expect("reopen");
    assert_eq!(
        reopened
            .verify_active_process_binding(&ActiveProcessBinding::from(&restored))
            .expect("verify after restart"),
        restored
    );
    assert!(matches!(
        reopened.verify_active_process_binding(&ActiveProcessBinding::from(&old)),
        Err(ProcessAuthorityError::StaleProcessBinding)
    ));
}

#[test]
fn durable_generation_and_binding_rows_are_ddl_immutable() {
    let root = TestRoot::new("immutable");
    let binding = {
        let authority = ProcessAuthority::open(root.path()).expect("open");
        let domain = authority
            .create_isolation_domain(domain_request(50))
            .expect("domain")
            .record()
            .clone();
        authority
            .register_delegated_process(registration(50, &domain))
            .expect("register")
            .record()
            .clone()
    };
    let raw = Connection::open(root.path().join("process-authority.db")).expect("raw open");
    assert!(
        raw.execute(
            "UPDATE process_bindings SET created_at_ms = created_at_ms + 1
             WHERE process_id = ?1 AND process_generation = ?2",
            rusqlite::params![
                binding.process_id.as_bytes().as_slice(),
                i64::try_from(binding.process_generation.get()).unwrap()
            ],
        )
        .is_err()
    );
    assert!(
        raw.execute(
            "DELETE FROM isolation_domain_generations WHERE isolation_domain_id = ?1",
            [binding.isolation_domain_id.as_bytes().as_slice()],
        )
        .is_err()
    );
}
