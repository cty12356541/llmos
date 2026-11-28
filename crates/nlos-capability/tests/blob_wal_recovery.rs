use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use cap_std::{ambient_authority, fs::Dir};
use ed25519_dalek::{Signer, SigningKey};
use nlos_capability::{
    CapabilityAuthority, CapabilityAuthorityError, CapabilityHandle, CapabilityRights,
    CapabilityTarget, IssueRootCapabilityRequest, SignedIssueRootCapabilityRequest,
    blob_reference::{
        AuthorizedWorkspace, BlobCommandScope, BlobConfig, BlobError, BlobReferenceService,
        BlobReply, RootedWorkspaceOpener, SignedMintCommand, SignedReleaseCommand,
        WorkspaceBinding, WorkspaceResolver, mint_command_message, mint_input_digest,
        read_input_digest, release_command_message, release_input_digest,
    },
    issue_root_command_message,
    service_call::{
        AuthorizationError, CallBinding, PolicyGrant, RuntimeCaller, ServiceAuthorizationBinding,
        ServiceAuthorizer, ServiceMethod, ServiceObject, ServiceTime,
    },
};
use nlos_identity::{BootstrapPrincipalRequest, IdentityAuthority, KeyPurpose};
use nlos_schema::{
    SABI_ENVELOPE_SCHEMA, blob_reference_schema_identity, decode_mint_reference_result,
    decode_read_reference_result, encode_mint_reference_request, encode_read_reference_request,
    encode_release_reference_request,
    sabi::v1::{
        CallerIdentity, Envelope, MintReferenceRequest, ReadReferenceRequest,
        ReleaseReferenceRequest, SabiRequestContext, SchemaIdentity, envelope::CommonContext,
    },
};
use nlos_store_fault::{FaultCode, FaultMode};
use nlos_types::{
    ApplicationId, Generation, IdempotencyKey, NamespaceId, PrincipalId, ProcessId, SessionId,
    WorkspaceId,
};
use rusqlite::{Connection, OpenFlags, ffi::SQLITE_IOERR, types::Value};
use sha2::{Digest, Sha256};

const VFS: &str = "nlos-blob-wal-fault";
const DATABASE: &str = "capability-authority.db";
const WAL: &str = "capability-authority.db-wal";
const FILE: &str = "chunk.bin";
const CONTENT: &[u8] = b"aabb";
const APP: ApplicationId = ApplicationId::from_bytes([3; 16]);
const WORKSPACE: WorkspaceBinding = WorkspaceBinding {
    workspace: WorkspaceId::from_bytes([1; 16]),
    namespace: NamespaceId::from_bytes([2; 16]),
    revision: Generation::INITIAL,
};
const RELEASED_MINT: u8 = 31;
const RELEASE: u8 = 32;
const ACTIVE_MINT: u8 = 33;
const FIRST_READ: u8 = 34;
const UNACKED_READ: u8 = 35;
const WAL_HEADER: usize = 32;
const FRAME_HEADER: usize = 24;

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("nlos-blob-wal-{}-{stamp}", std::process::id()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Actor {
    principal: PrincipalId,
    signing_key: SigningKey,
    capability: CapabilityHandle,
}

impl Actor {
    fn new(identity: &IdentityAuthority, authority: &CapabilityAuthority, seed: u8) -> Self {
        let signing_key = SigningKey::from_bytes(&[seed; 32]);
        let key = identity
            .bootstrap_principal(BootstrapPrincipalRequest {
                principal_profile_digest: [seed; 32],
                control_domain_policy_digest: [seed; 32],
                public_key: signing_key.verifying_key().to_bytes(),
                key_purpose: KeyPurpose::SemanticSigning,
                key_valid_from_ms: 1,
                key_valid_until_ms: 4000,
                idempotency_key: IdempotencyKey::from_bytes([seed; 16]),
                created_at_ms: 1,
            })
            .unwrap()
            .binding();
        let command = IssueRootCapabilityRequest {
            issuer_key_id: key.key_id,
            holder_key_id: key.key_id,
            target: CapabilityTarget::Namespace(WORKSPACE.namespace),
            rights: CapabilityRights::DELEGATE,
            purpose_digest: None,
            valid_from_ms: 1,
            valid_until_ms: 4000,
            delegation_depth_remaining: 1,
            call_limit: None,
            idempotency_key: IdempotencyKey::from_bytes([seed; 16]),
            issued_at_ms: 1,
        };
        let receipt = authority
            .issue_root_signed(
                identity,
                SignedIssueRootCapabilityRequest {
                    signer: key.principal_id,
                    signature: signing_key
                        .sign(&issue_root_command_message(command))
                        .to_bytes(),
                    command,
                },
            )
            .unwrap()
            .receipt();
        Self {
            principal: key.principal_id,
            signing_key,
            capability: CapabilityHandle {
                capability_id: receipt.capability_id,
                generation: receipt.generation,
            },
        }
    }

    fn call(&self, marker: u8) -> CallBinding {
        CallBinding {
            caller: RuntimeCaller {
                principal: self.principal,
                application: APP,
                process: ProcessId::from_bytes([4; 16]),
                process_generation: Generation::INITIAL,
                session: SessionId::from_bytes([5; 16]),
            },
            idempotency_key: Some(IdempotencyKey::from_bytes([marker; 16])),
            deadline_monotonic_ns: Some(30_000_000_000),
            capabilities: vec![self.capability],
        }
    }
}

struct Policy {
    authority: Arc<CapabilityAuthority>,
    issuer: PrincipalId,
    recipient: PrincipalId,
}

impl ServiceAuthorizer for Policy {
    fn authorize(
        &self,
        binding: &ServiceAuthorizationBinding,
        now: ServiceTime,
    ) -> Result<PolicyGrant, AuthorizationError> {
        let caller = binding.call.caller;
        let target = binding.action.target;
        let scope_matches = match (binding.action.method, target.object) {
            (ServiceMethod::MintReference, ServiceObject::Workspace(workspace)) => {
                caller.principal == self.issuer
                    && workspace == WORKSPACE.workspace
                    && target.generation == WORKSPACE.revision
            }
            (ServiceMethod::ReadReference, ServiceObject::BlobReference(_)) => {
                caller.principal == self.recipient
            }
            (ServiceMethod::ReleaseReference, ServiceObject::BlobReference(_)) => {
                caller.principal == self.issuer
            }
            _ => false,
        };
        if caller.application != APP || target.namespace != WORKSPACE.namespace || !scope_matches {
            return Err(AuthorizationError::Denied);
        }
        let [capability] = binding.call.capabilities.as_slice() else {
            return Err(AuthorizationError::Denied);
        };
        let record = self
            .authority
            .inspect_active(*capability, now.wall_ms)
            .map_err(|_| AuthorizationError::Denied)?;
        if record.holder != caller.principal
            || record.target != CapabilityTarget::Namespace(target.namespace)
        {
            return Err(AuthorizationError::Denied);
        }
        Ok(PolicyGrant {
            binding: binding.clone(),
            capability: *capability,
            valid_from_ms: 1,
            valid_until_ms: 4000,
        })
    }
}

struct Workspace {
    directory: Dir,
    issuer: PrincipalId,
    resolutions: Arc<AtomicUsize>,
}

impl WorkspaceResolver for Workspace {
    fn resolve(
        &self,
        caller: &RuntimeCaller,
        workspace: WorkspaceBinding,
    ) -> Result<AuthorizedWorkspace, BlobError> {
        self.resolutions.fetch_add(1, Ordering::Relaxed);
        if caller.principal != self.issuer || workspace != WORKSPACE {
            return Err(AuthorizationError::Denied.into());
        }
        Ok(AuthorizedWorkspace::managed_local(
            workspace,
            self.directory.try_clone()?,
        ))
    }
}

fn envelope(call: &CallBinding, method: &str, payload: Vec<u8>, digest: [u8; 32]) -> Envelope {
    let actor = call.caller;
    Envelope {
        schema: Some(SchemaIdentity {
            name: SABI_ENVELOPE_SCHEMA.to_owned(),
            major: 1,
            ..SchemaIdentity::default()
        }),
        request_id: vec![6; 16],
        service: "blob_reference".to_owned(),
        method: method.to_owned(),
        common_context: Some(CommonContext::RequestContext(SabiRequestContext {
            caller: Some(CallerIdentity {
                principal_id: actor.principal.as_bytes().to_vec(),
                application_id: actor.application.as_bytes().to_vec(),
                process_id: actor.process.as_bytes().to_vec(),
                process_generation: actor.process_generation.get(),
            }),
            correlation_id: vec![7; 16],
            idempotency_key: call.idempotency_key.unwrap().as_bytes().to_vec(),
            deadline_monotonic_ns: call.deadline_monotonic_ns.unwrap(),
            capability_handles: vec![nlos_schema::sabi::v1::CapabilityHandle {
                slot: 1,
                generation: 1,
            }],
            proposal_or_input_digest_sha256: digest.to_vec(),
            ..SabiRequestContext::default()
        })),
        payload,
    }
}

struct Fixture {
    identity: Arc<IdentityAuthority>,
    issuer: Actor,
    recipient: Actor,
    resolutions: Arc<AtomicUsize>,
    scratch: Scratch,
}

impl Fixture {
    fn new() -> (Self, Arc<CapabilityAuthority>) {
        let scratch = Scratch::new();
        let root = scratch.0.join("source");
        fs::create_dir(&root).unwrap();
        let workspace = scratch.0.join("workspace");
        fs::create_dir(&workspace).unwrap();
        fs::write(workspace.join(FILE), CONTENT).unwrap();

        nlos_store_fault::register(VFS).unwrap();
        let file = root.join(DATABASE).to_string_lossy().replace('\\', "/");
        // The authority appends its database filename. The URI fragment keeps
        // that suffix out of the actual SQLite filename and VFS parameters.
        let uri = format!("file:///{}?vfs={VFS}#", file.trim_start_matches('/'));
        let authority = Arc::new(CapabilityAuthority::open(uri).unwrap());
        let identity = Arc::new(IdentityAuthority::open(scratch.0.join("identity")).unwrap());
        let issuer = Actor::new(&identity, &authority, 11);
        let recipient = Actor::new(&identity, &authority, 12);
        (
            Self {
                identity,
                issuer,
                recipient,
                resolutions: Arc::default(),
                scratch,
            },
            authority,
        )
    }

    fn service(&self, authority: Arc<CapabilityAuthority>) -> BlobReferenceService {
        BlobReferenceService::new(
            Arc::clone(&authority),
            Arc::clone(&self.identity),
            Arc::new(Policy {
                authority,
                issuer: self.issuer.principal,
                recipient: self.recipient.principal,
            }),
            Arc::new(Workspace {
                directory: Dir::open_ambient_dir(
                    self.scratch.0.join("workspace"),
                    ambient_authority(),
                )
                .unwrap(),
                issuer: self.issuer.principal,
                resolutions: Arc::clone(&self.resolutions),
            }),
            RootedWorkspaceOpener,
            Arc::new(|| {
                Ok(ServiceTime {
                    wall_ms: 100,
                    monotonic_ns: 100_000_000,
                })
            }),
            BlobConfig::default(),
        )
        .unwrap()
    }

    fn scope(&self) -> BlobCommandScope {
        BlobCommandScope {
            workspace: WORKSPACE,
            source: self.issuer.capability,
            issued_at_ms: 99,
        }
    }

    fn mint(&self, service: &BlobReferenceService, marker: u8) -> BlobReply {
        let call = self.issuer.call(marker);
        let request = MintReferenceRequest {
            schema: Some(blob_reference_schema_identity()),
            path_handle: FILE.to_owned(),
            file_digest_sha256: Sha256::digest(CONTENT).to_vec(),
            size_bytes: CONTENT.len() as u64,
            max_reads: 3,
            max_chunk_bytes: 2,
            valid_until_ms: 1000,
            recipient_principal_id: self.recipient.principal.as_bytes().to_vec(),
            command_proof: None,
        };
        let scope = self.scope();
        let proof = SignedMintCommand {
            scope,
            signer: self.issuer.principal,
            signature: self
                .issuer
                .signing_key
                .sign(&mint_command_message(
                    &call.caller,
                    call.idempotency_key.unwrap(),
                    &request,
                    scope,
                ))
                .to_bytes(),
        };
        let wire = envelope(
            &call,
            "mint_reference",
            encode_mint_reference_request(&request).unwrap(),
            mint_input_digest(&request, scope),
        );
        service.mint_reference(&call, &wire, Some(&proof)).unwrap()
    }

    fn read(
        &self,
        service: &BlobReferenceService,
        reference: &[u8],
        offset: u64,
        marker: u8,
    ) -> Result<BlobReply, BlobError> {
        let call = self.recipient.call(marker);
        let request = ReadReferenceRequest {
            schema: Some(blob_reference_schema_identity()),
            reference_id: reference.to_vec(),
            offset,
            length_bytes: 2,
        };
        let wire = envelope(
            &call,
            "read_reference",
            encode_read_reference_request(&request).unwrap(),
            read_input_digest(&request),
        );
        service.read_reference(&call, &wire)
    }

    fn release(&self, service: &BlobReferenceService, reference: &[u8]) -> BlobReply {
        let call = self.issuer.call(RELEASE);
        let request = ReleaseReferenceRequest {
            schema: Some(blob_reference_schema_identity()),
            reference_id: reference.to_vec(),
            command_proof: None,
        };
        let scope = self.scope();
        let proof = SignedReleaseCommand {
            scope,
            signer: self.issuer.principal,
            signature: self
                .issuer
                .signing_key
                .sign(&release_command_message(
                    &call.caller,
                    call.idempotency_key.unwrap(),
                    &request,
                    scope,
                ))
                .to_bytes(),
        };
        let wire = envelope(
            &call,
            "release_reference",
            encode_release_reference_request(&request).unwrap(),
            release_input_digest(&request, scope),
        );
        service
            .release_reference(&call, &wire, Some(&proof))
            .unwrap()
    }
}

fn observer(root: &Path) -> Connection {
    Connection::open_with_flags(root.join(DATABASE), OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap()
}

#[derive(Debug, PartialEq)]
struct Ledger {
    tokens: Vec<Vec<Value>>,
    reads: Vec<Vec<Value>>,
    revocations: Vec<Vec<Value>>,
    usage: (i64, i64),
}

impl Ledger {
    fn snapshot(db: &Connection) -> Self {
        fn rows(db: &Connection, sql: &str) -> Vec<Vec<Value>> {
            let mut query = db.prepare(sql).unwrap();
            let columns = query.column_count();
            query
                .query_map([], |row| {
                    (0..columns).map(|column| row.get(column)).collect()
                })
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        }
        Self {
            tokens: rows(db, "SELECT * FROM blob_tokens ORDER BY reference_id"),
            reads: rows(
                db,
                "SELECT * FROM blob_read_receipts ORDER BY reference_id, ordinal",
            ),
            revocations: rows(db, "SELECT * FROM blob_revocations ORDER BY reference_id"),
            usage: db
                .query_row(
                    "SELECT rows, estimated_bytes FROM blob_audit_usage",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap(),
        }
    }
}

struct Prefix {
    released: BlobReply,
    released_reference: Vec<u8>,
    revoke: BlobReply,
    active: BlobReply,
    active_reference: Vec<u8>,
    ledger: Ledger,
}

impl Prefix {
    fn commit(fixture: &Fixture, service: &BlobReferenceService, db: &Connection) -> Self {
        let released = fixture.mint(service, RELEASED_MINT);
        let released_reference = decode_mint_reference_result(&released.payload)
            .unwrap()
            .reference_id;
        let revoke = fixture.release(service, &released_reference);
        let active = fixture.mint(service, ACTIVE_MINT);
        let active_reference = decode_mint_reference_result(&active.payload)
            .unwrap()
            .reference_id;
        let read = fixture
            .read(service, &active_reference, 0, FIRST_READ)
            .unwrap();
        assert_eq!(
            decode_read_reference_result(&read.payload).unwrap().chunk,
            b"aa"
        );
        assert_eq!(
            fixture
                .read(service, &active_reference, 0, FIRST_READ)
                .unwrap(),
            read
        );

        let ledger = Ledger::snapshot(db);
        assert_eq!(ledger.tokens.len(), 2);
        assert_eq!(ledger.revocations.len(), 1);
        assert_eq!(ledger.reads.len(), 1);
        assert_eq!(ledger.usage, (4, 10_368));
        let (receipt, ordinal, key): (Vec<u8>, i64, Vec<u8>) = db.query_row(
            "SELECT receipt_id, ordinal, idempotency_key FROM blob_read_receipts WHERE reference_id=?1",
            [&active_reference],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).unwrap();
        assert_eq!(receipt, read.receipt_id.as_bytes());
        assert_eq!(ordinal, 1);
        assert_eq!(key, [FIRST_READ; 16]);
        Self {
            released,
            released_reference,
            revoke,
            active,
            active_reference,
            ledger,
        }
    }

    fn assert_recovered(&self, fixture: &Fixture, root: &Path) {
        let authority = Arc::new(CapabilityAuthority::open(root).unwrap());
        let db = observer(root);
        assert_eq!(Ledger::snapshot(&db), self.ledger);
        let epoch = || {
            db.query_row("SELECT epoch FROM blob_boot", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap()
        };
        assert_eq!(epoch(), 1);
        let service = fixture.service(authority);
        assert_eq!(epoch(), 2);
        let resolutions = fixture.resolutions.load(Ordering::Relaxed);
        assert!(resolutions > 0, "the real mint used the workspace");
        assert_eq!(fixture.mint(&service, RELEASED_MINT), self.released);
        assert_eq!(fixture.mint(&service, ACTIVE_MINT), self.active);
        assert_eq!(
            fixture.release(&service, &self.released_reference),
            self.revoke
        );
        for (offset, key) in [(0, FIRST_READ), (2, UNACKED_READ)] {
            assert!(matches!(
                fixture.read(&service, &self.active_reference, offset, key),
                Err(BlobError::NotFound)
            ));
        }
        assert_eq!(fixture.resolutions.load(Ordering::Relaxed), resolutions);
        assert_eq!(Ledger::snapshot(&db), self.ledger);
        let mut integrity = db.prepare("PRAGMA integrity_check").unwrap();
        let results: Vec<String> = integrity
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(results, ["ok"]);
    }
}

fn wal_u32(bytes: &[u8], offset: usize) -> usize {
    u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize
}

fn assert_torn_read(prefix: &[u8], wal: &[u8], error: &BlobError, writes: u64) -> usize {
    let BlobError::Authority(CapabilityAuthorityError::Sqlite(rusqlite::Error::SqliteFailure(
        error,
        _,
    ))) = error
    else {
        panic!("expected SQLite write error, got {error:?}");
    };
    assert_eq!(error.extended_code, SQLITE_IOERR);
    assert_eq!(writes, 4);

    let page = wal_u32(prefix, 8);
    assert!(matches!(page, 512..=65_536) && page.is_power_of_two());
    let frame = FRAME_HEADER + page;
    let prefix_end = prefix.len();
    assert!(prefix_end > WAL_HEADER);
    assert_eq!((prefix_end - WAL_HEADER) % frame, 0);
    // SQLite's frame header stores the post-commit DB page count, or zero for
    // non-commit frames. The byte boundary below retains the last ACKed frame.
    let prefix_frames = (prefix_end - WAL_HEADER) / frame;
    assert_ne!(wal_u32(prefix, prefix_end - frame + 4), 0);
    assert_eq!(&wal[..prefix_end], prefix);
    assert_eq!(wal.len(), prefix_end + frame + FRAME_HEADER);
    assert_eq!(wal_u32(wal, prefix_end + 4), 0);
    assert_ne!(wal_u32(wal, prefix_end + frame), 0);
    assert_eq!(wal_u32(wal, prefix_end + frame + 4), 0);

    let corrupt_at = prefix_end + FRAME_HEADER + 64;
    eprintln!(
        "Blob read COMMIT: rc={}, xWrite calls={writes}, page bytes={page}, \
         ACKed through frame {prefix_frames} / byte {prefix_end} (exclusive), \
         one complete uncommitted frame, next frame header only, \
         first missing page starts at byte {}, checksum probe flips byte {corrupt_at}",
        error.extended_code,
        wal.len(),
    );
    corrupt_at
}

fn write_recovery_image(root: &Path, main: &[u8], wal: &[u8]) {
    fs::create_dir(root).unwrap();
    fs::write(root.join(DATABASE), main).unwrap();
    fs::write(root.join(WAL), wal).unwrap();
    // A crash-image copy deliberately omits the original live WAL index.
    assert!(!root.join("capability-authority.db-shm").exists());
}

#[test]
fn failed_blob_read_commit_recovers_only_the_acknowledged_wal_prefix() {
    let (fixture, authority) = Fixture::new();
    let source = fixture.scratch.0.join("source");
    let service = fixture.service(Arc::clone(&authority));
    let db = observer(&source);
    // Keep grants, schema and boot in the main file. The later signed Blob
    // mutations must be recovered from the WAL rather than from a checkpoint.
    let checkpoint: (i64, i64, i64) = db
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .unwrap();
    assert_eq!(checkpoint, (0, 0, 0));
    let main = fs::read(source.join(DATABASE)).unwrap();
    let prefix = Prefix::commit(&fixture, &service, &db);
    let acked_wal = fs::read(source.join(WAL)).unwrap();

    // Only this test uses the registered VFS. Pass through one frame and the
    // next header, then return an actual SQLite xWrite error during COMMIT.
    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 3,
        code: FaultCode::IoErr,
    });
    let failed = fixture.read(&service, &prefix.active_reference, 2, UNACKED_READ);
    let writes = nlos_store_fault::writes_observed();
    nlos_store_fault::disarm();
    let error = failed.expect_err("a torn commit must never ACK the read");

    // Single writer, no concurrent activity or checkpoint between these reads.
    // Capture before closing SQLite, which is allowed to checkpoint on close.
    let mut torn_wal = fs::read(source.join(WAL)).unwrap();
    let corrupt_at = assert_torn_read(&acked_wal, &torn_wal, &error, writes);
    assert_eq!(fs::read(source.join(DATABASE)).unwrap(), main);
    assert_eq!(Ledger::snapshot(&db), prefix.ledger);

    let torn = fixture.scratch.0.join("torn");
    write_recovery_image(&torn, &main, &torn_wal);
    torn_wal[corrupt_at] ^= 0xA5;
    let corrupted = fixture.scratch.0.join("corrupted-uncommitted-frame");
    write_recovery_image(&corrupted, &main, &torn_wal);

    drop(db);
    drop(service);
    drop(authority);
    fs::remove_file(fixture.scratch.0.join("workspace").join(FILE)).unwrap();

    // Reopen both the real post-error DB and the images that preserve the
    // uncheckpointed WAL, including a checksum break strictly after the ACKs.
    for root in [&source, &torn, &corrupted] {
        prefix.assert_recovered(&fixture, root);
    }
}
