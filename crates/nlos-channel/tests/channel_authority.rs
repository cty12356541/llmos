use nlos_channel::{
    ChannelAuthority, ChannelDecision, ChannelRotationDecision, CreateChannelRequest,
    RotateChannelRequest,
};
use nlos_types::{Generation, IdempotencyKey};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

fn key(seed: u8) -> IdempotencyKey {
    IdempotencyKey::from_bytes([seed; 16])
}

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

struct Root(PathBuf);

impl Root {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let sequence = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!(
            "nlos-channel-{label}-{}-{nonce}-{sequence}",
            std::process::id()
        )))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn channel_endpoint_proof_is_owner_assigned_and_survives_restart() {
    let root = Root::new("proof-restart");
    let (record, proof, rotated, rotated_proof) = {
        let authority = ChannelAuthority::open(root.path()).expect("open authority");
        let request = CreateChannelRequest {
            capacity_bytes: 4096,
            policy_digest: [0x11; 32],
            idempotency_key: key(1),
            created_at_ms: 10,
        };
        let record = match authority.create_channel(request).expect("create") {
            ChannelDecision::Created(record) => record,
            ChannelDecision::Replayed(_) => panic!("first create cannot replay"),
        };
        let replay = authority
            .create_channel(request)
            .expect("create replay")
            .record();
        assert_eq!(replay, record);
        let proof = authority
            .inspect_endpoint_proof(record.channel_id)
            .expect("initial proof");
        assert_eq!(proof.participant_generation, Generation::INITIAL);
        assert_eq!(proof.channel_id, record.channel_id);

        let rotated = match authority
            .rotate_channel(RotateChannelRequest {
                channel_id: record.channel_id,
                expected_generation: record.generation,
                expected_fencing_token: record.fencing_token,
                idempotency_key: key(2),
                rotated_at_ms: 20,
            })
            .expect("rotate")
        {
            ChannelRotationDecision::Rotated(record) => record,
            ChannelRotationDecision::Replayed(_) => panic!("first rotate cannot replay"),
        };
        assert_eq!(rotated.generation.get(), 2);
        assert_ne!(rotated.fencing_token, record.fencing_token);
        let rotated_proof = authority
            .inspect_endpoint_proof(record.channel_id)
            .expect("rotated proof");
        assert_eq!(rotated_proof.participant_id, proof.participant_id);
        assert_eq!(rotated_proof.participant_generation, rotated.generation);
        assert_ne!(
            rotated_proof.admission_receipt_id,
            proof.admission_receipt_id
        );
        (record, proof, rotated, rotated_proof)
    };

    let reopened = ChannelAuthority::open(root.path()).expect("reopen authority");
    assert_eq!(
        reopened.inspect_channel(record.channel_id).expect("head"),
        rotated
    );
    assert_eq!(
        reopened
            .inspect_endpoint_proof(record.channel_id)
            .expect("proof readback"),
        rotated_proof
    );
    assert_eq!(
        reopened
            .rotate_channel(RotateChannelRequest {
                channel_id: record.channel_id,
                expected_generation: record.generation,
                expected_fencing_token: record.fencing_token,
                idempotency_key: key(2),
                rotated_at_ms: 20,
            })
            .expect("rotation replay")
            .record(),
        rotated
    );
    assert_eq!(proof.participant_id, rotated_proof.participant_id);
}

#[test]
fn stale_generation_and_idempotency_conflicts_fail_closed() {
    let root = Root::new("fence");
    let authority = ChannelAuthority::open(root.path()).expect("open authority");
    let request = CreateChannelRequest {
        capacity_bytes: 512,
        policy_digest: [0x22; 32],
        idempotency_key: key(3),
        created_at_ms: 30,
    };
    let record = authority.create_channel(request).expect("create").record();
    let conflicting = authority.create_channel(CreateChannelRequest {
        capacity_bytes: 513,
        ..request
    });
    assert!(matches!(
        conflicting,
        Err(nlos_channel::ChannelAuthorityError::IdempotencyConflict)
    ));

    let rotated = authority
        .rotate_channel(RotateChannelRequest {
            channel_id: record.channel_id,
            expected_generation: record.generation,
            expected_fencing_token: record.fencing_token,
            idempotency_key: key(4),
            rotated_at_ms: 40,
        })
        .expect("rotate")
        .record();
    assert!(matches!(
        authority.rotate_channel(RotateChannelRequest {
            channel_id: record.channel_id,
            expected_generation: record.generation,
            expected_fencing_token: record.fencing_token,
            idempotency_key: key(5),
            rotated_at_ms: 50,
        }),
        Err(nlos_channel::ChannelAuthorityError::StaleChannel)
    ));
    assert!(matches!(
        authority.rotate_channel(RotateChannelRequest {
            channel_id: record.channel_id,
            expected_generation: rotated.generation,
            expected_fencing_token: rotated.fencing_token,
            idempotency_key: key(4),
            rotated_at_ms: 41,
        }),
        Err(nlos_channel::ChannelAuthorityError::IdempotencyConflict)
    ));
}

#[test]
fn zero_capacity_is_rejected_before_durable_write() {
    let root = Root::new("capacity");
    let authority = ChannelAuthority::open(root.path()).expect("open authority");
    let result = authority.create_channel(CreateChannelRequest {
        capacity_bytes: 0,
        policy_digest: [0x33; 32],
        idempotency_key: key(6),
        created_at_ms: 60,
    });
    assert!(matches!(
        result,
        Err(nlos_channel::ChannelAuthorityError::InvalidCapacity)
    ));
}
