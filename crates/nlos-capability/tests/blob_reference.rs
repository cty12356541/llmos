use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use cap_std::{ambient_authority, fs::Dir};
use ed25519_dalek::{Signer, SigningKey};
use nlos_capability::{
    CapabilityAuthority, CapabilityHandle, CapabilityRights, CapabilityTarget,
    IssueRootCapabilityRequest, SignedIssueRootCapabilityRequest,
    blob_reference::{
        AuthorizedWorkspace, BlobClock, BlobCommandProof, BlobCommandScope, BlobConfig, BlobError,
        BlobReferenceService, BlobReply, RootedWorkspaceOpener, SignedMintCommand,
        SignedReleaseCommand, WorkspaceBinding, WorkspaceResolver, mint_command_message,
        mint_input_digest, read_input_digest, release_command_message, release_input_digest,
    },
    issue_root_command_message,
    service_call::{
        AuthorizationError, CallBinding, PolicyGrant, RuntimeCaller, ServiceAuthorizationBinding,
        ServiceAuthorizer, ServiceMethod, ServiceObject, ServiceTime,
    },
};
use nlos_identity::{BootstrapPrincipalRequest, IdentityAuthority, IdentityBinding, KeyPurpose};
use nlos_schema::{
    SABI_ENVELOPE_SCHEMA, blob_reference_schema_identity, decode_mint_reference_result,
    decode_read_reference_result, encode_mint_reference_request, encode_read_reference_request,
    encode_release_reference_request,
    sabi::v1::{
        CallerIdentity, Envelope, MintReferenceRequest, ReadReferenceRequest,
        ReleaseReferenceRequest, SabiRequestContext, SchemaIdentity, envelope::CommonContext,
    },
};
use nlos_types::{
    ApplicationId, Generation, IdempotencyKey, NamespaceId, PrincipalId, ProcessId, SessionId,
    WorkspaceId,
};
use sha2::{Digest, Sha256};

const WORKSPACE: WorkspaceBinding = WorkspaceBinding {
    workspace: WorkspaceId::from_bytes([1; 16]),
    namespace: NamespaceId::from_bytes([2; 16]),
    revision: Generation::INITIAL,
};
const APP: ApplicationId = ApplicationId::from_bytes([3; 16]);

struct LocalWorkspace {
    issuers: [PrincipalId; 2],
    directory: Dir,
    resolutions: Arc<AtomicUsize>,
}

impl WorkspaceResolver for LocalWorkspace {
    fn resolve(
        &self,
        caller: &RuntimeCaller,
        workspace: WorkspaceBinding,
    ) -> Result<AuthorizedWorkspace, BlobError> {
        if !self.issuers.contains(&caller.principal) || workspace != WORKSPACE {
            return Err(BlobError::Authorization(AuthorizationError::Denied));
        }
        self.resolutions.fetch_add(1, Ordering::SeqCst);
        Ok(AuthorizedWorkspace::managed_local(
            WORKSPACE,
            self.directory.try_clone()?,
        ))
    }
}

type PolicyHook = dyn Fn(&ServiceAuthorizationBinding) + Send + Sync;
type ClockHook = dyn Fn() + Send + Sync;

struct Policy {
    issuer: PrincipalId,
    holder: PrincipalId,
    issuer_handle: CapabilityHandle,
    holder_handle: CapabilityHandle,
    third: PrincipalId,
    third_handle: CapabilityHandle,
    authority: Arc<CapabilityAuthority>,
    denied: Arc<AtomicBool>,
    hook: Arc<Mutex<Option<Arc<PolicyHook>>>>,
}

impl ServiceAuthorizer for Policy {
    fn authorize(
        &self,
        binding: &ServiceAuthorizationBinding,
        now: ServiceTime,
    ) -> Result<PolicyGrant, AuthorizationError> {
        let caller = binding.call.caller;
        let target = binding.action.target;
        if self.denied.load(Ordering::SeqCst)
            || caller.application != APP
            || target.namespace != WORKSPACE.namespace
        {
            return Err(AuthorizationError::Denied);
        }
        match (binding.action.method, target.object) {
            (ServiceMethod::MintReference, ServiceObject::Workspace(workspace))
                if [self.issuer, self.third].contains(&caller.principal)
                    && workspace == WORKSPACE.workspace
                    && target.generation == WORKSPACE.revision => {}
            (
                ServiceMethod::ReadReference | ServiceMethod::ReleaseReference,
                ServiceObject::BlobReference(_),
            ) => (),
            _ => return Err(AuthorizationError::Denied),
        }
        let capability = [
            (self.issuer, self.issuer_handle),
            (self.holder, self.holder_handle),
            (self.third, self.third_handle),
        ]
        .into_iter()
        .find_map(|(principal, handle)| (principal == caller.principal).then_some(handle))
        .ok_or(AuthorizationError::Denied)?;
        let record = self
            .authority
            .inspect_active(capability, now.wall_ms)
            .map_err(|_| AuthorizationError::Denied)?;
        if record.holder != caller.principal
            || record.target != CapabilityTarget::Namespace(target.namespace)
        {
            return Err(AuthorizationError::Denied);
        }
        let hook = self.hook.lock().unwrap().clone();
        if let Some(hook) = hook {
            hook(binding);
        }
        Ok(PolicyGrant {
            binding: binding.clone(),
            capability,
            valid_from_ms: 1,
            valid_until_ms: 3_600_000,
        })
    }
}

fn caller(principal: PrincipalId, handle: CapabilityHandle, marker: u8) -> CallBinding {
    CallBinding {
        caller: RuntimeCaller {
            principal,
            application: APP,
            process: ProcessId::from_bytes([4; 16]),
            process_generation: Generation::INITIAL,
            session: SessionId::from_bytes([5; 16]),
        },
        idempotency_key: Some(IdempotencyKey::from_bytes([marker; 16])),
        deadline_monotonic_ns: Some(100_000_000_000_000),
        capabilities: vec![handle],
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

fn principal(identity: &IdentityAuthority, seed: u8) -> (SigningKey, IdentityBinding) {
    let signing = SigningKey::from_bytes(&[seed; 32]);
    let binding = identity
        .bootstrap_principal(BootstrapPrincipalRequest {
            principal_profile_digest: [seed; 32],
            control_domain_policy_digest: [seed; 32],
            public_key: signing.verifying_key().to_bytes(),
            key_purpose: KeyPurpose::SemanticSigning,
            key_valid_from_ms: 1,
            key_valid_until_ms: 3_600_000,
            idempotency_key: IdempotencyKey::from_bytes([seed; 16]),
            created_at_ms: 1,
        })
        .unwrap()
        .binding();
    (signing, binding)
}

fn grant(
    authority: &CapabilityAuthority,
    identity: &IdentityAuthority,
    signing: &SigningKey,
    holder: IdentityBinding,
    marker: u8,
) -> CapabilityHandle {
    let command = IssueRootCapabilityRequest {
        issuer_key_id: holder.key_id,
        holder_key_id: holder.key_id,
        target: CapabilityTarget::Namespace(WORKSPACE.namespace),
        rights: CapabilityRights::DELEGATE,
        purpose_digest: None,
        valid_from_ms: 1,
        valid_until_ms: 3_600_000,
        delegation_depth_remaining: 1,
        call_limit: None,
        idempotency_key: IdempotencyKey::from_bytes([marker; 16]),
        issued_at_ms: 1,
    };
    let receipt = authority
        .issue_root_signed(
            identity,
            SignedIssueRootCapabilityRequest {
                signer: holder.principal_id,
                signature: signing
                    .sign(&issue_root_command_message(command))
                    .to_bytes(),
                command,
            },
        )
        .unwrap()
        .receipt();
    CapabilityHandle {
        capability_id: receipt.capability_id,
        generation: receipt.generation,
    }
}

static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);
static FAULT_LOCK: Mutex<()> = Mutex::new(());
const FAULT_VFS: &str = "nlos-blob-fault";

fn fault_uri(directory: &std::path::Path, vfs: &str) -> String {
    let database = directory.join("capability-authority.db");
    let uri = database.to_string_lossy().replace('\\', "/");
    format!("file:///{}?vfs={vfs}&tail=", uri.trim_start_matches('/'))
}

struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "nlos-blob-{}-{}",
            std::process::id(),
            NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join("workspace/fixtures")).unwrap();
        Self(root)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Fixture {
    authority: Arc<CapabilityAuthority>,
    identity: Arc<IdentityAuthority>,
    issuer: IdentityBinding,
    sender_key: SigningKey,
    holder: IdentityBinding,
    recipient_key: SigningKey,
    third: IdentityBinding,
    third_key: SigningKey,
    source: CapabilityHandle,
    holder_handle: CapabilityHandle,
    third_handle: CapabilityHandle,
    clock: Arc<AtomicU64>,
    clock_failed: Arc<AtomicBool>,
    denied: Arc<AtomicBool>,
    resolutions: Arc<AtomicUsize>,
    hook: Arc<Mutex<Option<Arc<PolicyHook>>>>,
    clock_hook: Arc<Mutex<Option<Arc<ClockHook>>>>,
    root: Scratch,
}

impl Fixture {
    fn new() -> Self {
        Self::with_authority(|root| CapabilityAuthority::open(root).unwrap())
    }

    fn with_authority(open: impl FnOnce(&std::path::Path) -> CapabilityAuthority) -> Self {
        let root = Scratch::new();
        std::fs::create_dir_all(root.0.join("capability")).unwrap();
        let authority = Arc::new(open(&root.0.join("capability")));
        let identity = Arc::new(IdentityAuthority::open(root.0.join("identity")).unwrap());
        let (sender_key, issuer) = principal(&identity, 11);
        let (recipient_key, holder) = principal(&identity, 12);
        let (third_key, third) = principal(&identity, 13);
        let source = grant(&authority, &identity, &sender_key, issuer, 21);
        let holder_handle = grant(&authority, &identity, &recipient_key, holder, 22);
        let third_handle = grant(&authority, &identity, &third_key, third, 23);
        Self {
            authority,
            identity,
            issuer,
            sender_key,
            holder,
            recipient_key,
            third,
            third_key,
            source,
            holder_handle,
            third_handle,
            clock: Arc::new(AtomicU64::new(100)),
            clock_failed: Arc::new(AtomicBool::new(false)),
            denied: Arc::new(AtomicBool::new(false)),
            resolutions: Arc::new(AtomicUsize::new(0)),
            hook: Arc::new(Mutex::new(None)),
            clock_hook: Arc::new(Mutex::new(None)),
            root,
        }
    }

    fn service(&self, config: BlobConfig) -> Result<BlobReferenceService, BlobError> {
        self.service_with_authority(Arc::clone(&self.authority), config)
    }

    fn service_with_authority(
        &self,
        authority: Arc<CapabilityAuthority>,
        config: BlobConfig,
    ) -> Result<BlobReferenceService, BlobError> {
        let clock = Arc::clone(&self.clock);
        let failed = Arc::clone(&self.clock_failed);
        let clock_hook = Arc::clone(&self.clock_hook);
        self.service_with_clock(
            authority,
            config,
            Arc::new(move || {
                let hook = clock_hook.lock().unwrap().clone();
                if let Some(hook) = hook {
                    hook();
                }
                if failed.load(Ordering::SeqCst) {
                    return Err(AuthorizationError::TimeUnavailable);
                }
                let wall_ms = clock.load(Ordering::SeqCst);
                Ok(ServiceTime {
                    wall_ms,
                    monotonic_ns: wall_ms * 1_000_000,
                })
            }),
        )
    }

    fn service_with_clock(
        &self,
        authority: Arc<CapabilityAuthority>,
        config: BlobConfig,
        clock: Arc<BlobClock>,
    ) -> Result<BlobReferenceService, BlobError> {
        BlobReferenceService::new(
            authority,
            Arc::clone(&self.identity),
            Arc::new(Policy {
                issuer: self.issuer.principal_id,
                holder: self.holder.principal_id,
                third: self.third.principal_id,
                issuer_handle: self.source,
                holder_handle: self.holder_handle,
                third_handle: self.third_handle,
                authority: Arc::clone(&self.authority),
                denied: Arc::clone(&self.denied),
                hook: Arc::clone(&self.hook),
            }),
            Arc::new(LocalWorkspace {
                issuers: [self.issuer.principal_id, self.third.principal_id],
                directory: Dir::open_ambient_dir(
                    self.root.0.join("workspace"),
                    ambient_authority(),
                )
                .unwrap(),
                resolutions: Arc::clone(&self.resolutions),
            }),
            RootedWorkspaceOpener,
            clock,
            config,
        )
    }

    fn db(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.db_path()).unwrap()
    }

    fn db_path(&self) -> std::path::PathBuf {
        self.root.0.join("capability/capability-authority.db")
    }

    fn file(&self) -> std::path::PathBuf {
        self.root.0.join("workspace/fixtures/tone.wav")
    }

    fn mint_request(&self, bytes: &[u8], block_size: u64, max_reads: u64) -> MintReferenceRequest {
        std::fs::write(self.file(), bytes).unwrap();
        MintReferenceRequest {
            schema: Some(blob_reference_schema_identity()),
            path_handle: "fixtures/tone.wav".to_owned(),
            file_digest_sha256: Sha256::digest(bytes).to_vec(),
            size_bytes: u64::try_from(bytes.len()).unwrap(),
            max_reads,
            max_chunk_bytes: block_size,
            valid_until_ms: 4_000,
            recipient_principal_id: self.holder.principal_id.as_bytes().to_vec(),
            command_proof: None,
        }
    }

    fn mint_scope(&self) -> BlobCommandScope {
        BlobCommandScope {
            workspace: WORKSPACE,
            source: self.source,
            issued_at_ms: 99,
        }
    }

    fn mint_call(&self, marker: u8) -> CallBinding {
        caller(self.issuer.principal_id, self.source, marker)
    }

    fn mint(
        &self,
        service: &BlobReferenceService,
        request: &MintReferenceRequest,
        marker: u8,
    ) -> Result<(BlobReply, Vec<u8>), BlobError> {
        let (call, request, proof) = self.signed_mint(request, marker);
        let reply = service.mint_reference(&call, &request, Some(&proof))?;
        let reference = decode_mint_reference_result(&reply.payload)
            .unwrap()
            .reference_id;
        Ok((reply, reference))
    }

    fn mint_as_third(
        &self,
        service: &BlobReferenceService,
        request: &MintReferenceRequest,
        marker: u8,
    ) -> Result<Vec<u8>, BlobError> {
        let call = caller(self.third.principal_id, self.third_handle, marker);
        let scope = BlobCommandScope {
            source: self.third_handle,
            ..self.mint_scope()
        };
        let proof = SignedMintCommand {
            scope,
            signer: self.third.principal_id,
            signature: self
                .third_key
                .sign(&mint_command_message(
                    &call.caller,
                    call.idempotency_key.unwrap(),
                    request,
                    scope,
                ))
                .to_bytes(),
        };
        let reply =
            service.mint_reference(&call, &mint_envelope(&call, request, scope), Some(&proof))?;
        Ok(decode_mint_reference_result(&reply.payload)
            .unwrap()
            .reference_id)
    }

    fn signed_mint(
        &self,
        request: &MintReferenceRequest,
        marker: u8,
    ) -> (CallBinding, Envelope, SignedMintCommand) {
        let call = self.mint_call(marker);
        let scope = self.mint_scope();
        let proof = SignedMintCommand {
            scope,
            signer: self.issuer.principal_id,
            signature: self
                .sender_key
                .sign(&mint_command_message(
                    &call.caller,
                    call.idempotency_key.unwrap(),
                    request,
                    scope,
                ))
                .to_bytes(),
        };
        let envelope = mint_envelope(&call, request, scope);
        (call, envelope, proof)
    }

    fn read_call(&self, marker: u8) -> CallBinding {
        caller(self.holder.principal_id, self.holder_handle, marker)
    }

    fn read(
        &self,
        service: &BlobReferenceService,
        reference: &[u8],
        offset: u64,
        length: u64,
        marker: u8,
    ) -> Result<BlobReply, BlobError> {
        let call = self.read_call(marker);
        service.read_reference(&call, &read_envelope(&call, reference, offset, length))
    }

    fn signed_release(
        &self,
        actor: IdentityBinding,
        reference: &[u8],
        marker: u8,
    ) -> (CallBinding, Envelope, SignedReleaseCommand) {
        let (signing, handle) = if actor == self.issuer {
            (&self.sender_key, self.source)
        } else if actor == self.holder {
            (&self.recipient_key, self.holder_handle)
        } else {
            (&self.third_key, self.third_handle)
        };
        let call = caller(actor.principal_id, handle, marker);
        let scope = self.mint_scope();
        let request = ReleaseReferenceRequest {
            schema: Some(blob_reference_schema_identity()),
            reference_id: reference.to_vec(),
            command_proof: None,
        };
        let proof = SignedReleaseCommand {
            scope,
            signer: actor.principal_id,
            signature: signing
                .sign(&release_command_message(
                    &call.caller,
                    call.idempotency_key.unwrap(),
                    &request,
                    scope,
                ))
                .to_bytes(),
        };
        let request = envelope(
            &call,
            "release_reference",
            encode_release_reference_request(&request).unwrap(),
            release_input_digest(&request, scope),
        );
        (call, request, proof)
    }

    fn reads(&self) -> i64 {
        self.db()
            .query_row("SELECT count(*) FROM blob_read_receipts", [], |row| {
                row.get(0)
            })
            .unwrap()
    }
}

fn mint_envelope(
    call: &CallBinding,
    request: &MintReferenceRequest,
    scope: BlobCommandScope,
) -> Envelope {
    envelope(
        call,
        "mint_reference",
        encode_mint_reference_request(request).unwrap(),
        mint_input_digest(request, scope),
    )
}

fn read_envelope(call: &CallBinding, reference: &[u8], offset: u64, length: u64) -> Envelope {
    let request = ReadReferenceRequest {
        schema: Some(blob_reference_schema_identity()),
        reference_id: reference.to_vec(),
        offset,
        length_bytes: length,
    };
    envelope(
        call,
        "read_reference",
        encode_read_reference_request(&request).unwrap(),
        read_input_digest(&request),
    )
}

#[test]
fn signed_sender_mint_roundtrips_wav_bytes_to_its_explicit_recipient() {
    let fixture = Fixture::new();
    let service = fixture.service(BlobConfig::default()).unwrap();
    let wav = b"RIFF\x28\0\0\0WAVEfmt \x10\0\0\0\x01\0\x01\0\x40\x1f\0\0\x80\x3e\0\0\x02\0\x10\0data\x04\0\0\0\x01\0\x02\0";
    let request = fixture.mint_request(wav, 17, 4);
    let (call, mint, signed) = fixture.signed_mint(&request, 31);
    assert!(matches!(
        service.mint_reference(&call, &mint, None),
        Err(BlobError::MissingSignature)
    ));
    let reply = service
        .handle_for_ipc(&call, &mint, Some(BlobCommandProof::Mint(&signed)))
        .unwrap();
    let receipt = decode_mint_reference_result(&reply.payload).unwrap();
    assert_eq!(receipt.minted_at_ms, 100);

    let mut reconstructed = Vec::new();
    for (index, offset) in (0..wav.len()).step_by(17).enumerate() {
        let reply = fixture
            .read(
                &service,
                &receipt.reference_id,
                offset as u64,
                17,
                35 + u8::try_from(index).unwrap(),
            )
            .unwrap();
        let block = decode_read_reference_result(&reply.payload).unwrap();
        assert_eq!(
            block.chunk_digest_sha256,
            Sha256::digest(&block.chunk).to_vec()
        );
        assert_eq!(block.offset, offset as u64);
        reconstructed.extend_from_slice(&block.chunk);
    }
    assert_eq!(reconstructed, wav);
    for call in [
        fixture.mint_call(50),
        caller(fixture.third.principal_id, fixture.third_handle, 50),
    ] {
        let request = read_envelope(&call, &receipt.reference_id, 0, 17);
        assert!(matches!(
            service.read_reference(&call, &request),
            Err(BlobError::NotFound)
        ));
    }
    assert_eq!(fixture.reads(), 3);
}

#[test]
fn untrusted_wire_actor_digest_and_task_context_are_rejected() {
    let fixture = Fixture::new();
    let service = fixture.service(BlobConfig::default()).unwrap();
    let request = fixture.mint_request(b"data", 2, 4);
    let (call, wire, signed) = fixture.signed_mint(&request, 31);
    let mut forged = wire.clone();
    let Some(CommonContext::RequestContext(context)) = &mut forged.common_context else {
        panic!("test request")
    };
    context.caller.as_mut().unwrap().process_generation += 1;
    assert!(matches!(
        service.mint_reference(&call, &forged, Some(&signed)),
        Err(BlobError::BindingMismatch)
    ));
    let mut forged = wire.clone();
    let Some(CommonContext::RequestContext(context)) = &mut forged.common_context else {
        panic!("test request")
    };
    context.task_execution_binding = Some(nlos_schema::sabi::v1::TaskExecutionBinding {
        task_attempt_id: vec![3; 16],
        task_authority_term: 1,
        isolation_domain_generation: 1,
        ..nlos_schema::sabi::v1::TaskExecutionBinding::default()
    });
    assert!(matches!(
        service.mint_reference(&call, &forged, Some(&signed)),
        Err(BlobError::UnsupportedBinding)
    ));
    let mut forged = wire;
    let Some(CommonContext::RequestContext(context)) = &mut forged.common_context else {
        panic!("test request")
    };
    context.proposal_or_input_digest_sha256 = vec![0; 32];
    assert!(matches!(
        service.mint_reference(&call, &forged, Some(&signed)),
        Err(BlobError::BindingMismatch)
    ));
    assert_eq!(fixture.resolutions.load(Ordering::SeqCst), 0);
}

#[test]
fn mint_signature_binds_file_fields_and_explicit_recipient() {
    let fixture = Fixture::new();
    let service = fixture.service(BlobConfig::default()).unwrap();
    let request = fixture.mint_request(b"data", 2, 4);
    let (call, _, signed) = fixture.signed_mint(&request, 31);
    let incorrect_requests = [
        MintReferenceRequest {
            path_handle: "fixtures/else.wav".to_string(),
            ..request.clone()
        },
        MintReferenceRequest {
            file_digest_sha256: vec![1; 32],
            ..request.clone()
        },
        MintReferenceRequest {
            size_bytes: 3,
            ..request.clone()
        },
        MintReferenceRequest {
            max_reads: 3,
            ..request.clone()
        },
        MintReferenceRequest {
            max_chunk_bytes: 3,
            ..request.clone()
        },
        MintReferenceRequest {
            valid_until_ms: 3_999,
            ..request.clone()
        },
        MintReferenceRequest {
            recipient_principal_id: fixture.third.principal_id.as_bytes().to_vec(),
            ..request.clone()
        },
    ];
    for incorrect in incorrect_requests {
        assert!(matches!(
            service.mint_reference(
                &call,
                &mint_envelope(&call, &incorrect, signed.scope),
                Some(&signed)
            ),
            Err(BlobError::Authority(
                nlos_capability::CapabilityAuthorityError::SignatureInvalid
            ))
        ));
    }
    assert_eq!(fixture.resolutions.load(Ordering::SeqCst), 0);
}

#[test]
fn mint_signature_binds_workspace_source_key_and_command_time() {
    let fixture = Fixture::new();
    let service = fixture.service(BlobConfig::default()).unwrap();
    let request = fixture.mint_request(b"data", 2, 4);
    let (call, wire, signed) = fixture.signed_mint(&request, 31);
    let wrong_scopes = [
        BlobCommandScope {
            issued_at_ms: 98,
            ..signed.scope
        },
        BlobCommandScope {
            workspace: WorkspaceBinding {
                namespace: NamespaceId::from_bytes([27; 16]),
                ..WORKSPACE
            },
            ..signed.scope
        },
        BlobCommandScope {
            workspace: WorkspaceBinding {
                workspace: WorkspaceId::from_bytes([28; 16]),
                ..WORKSPACE
            },
            ..signed.scope
        },
        BlobCommandScope {
            workspace: WorkspaceBinding {
                revision: Generation::INITIAL.checked_next().unwrap(),
                ..WORKSPACE
            },
            ..signed.scope
        },
        BlobCommandScope {
            source: fixture.holder_handle,
            ..signed.scope
        },
        BlobCommandScope {
            source: CapabilityHandle {
                generation: Generation::INITIAL.checked_next().unwrap(),
                ..signed.scope.source
            },
            ..signed.scope
        },
    ];
    for scope in wrong_scopes {
        let proof = SignedMintCommand { scope, ..signed };
        let message =
            mint_command_message(&call.caller, call.idempotency_key.unwrap(), &request, scope);
        assert!(matches!(
            fixture.identity.verify_capability_command_signature(
                nlos_identity::VerifyCapabilityCommandSignatureRequest {
                    message_digest: message,
                    principal: fixture.issuer.principal_id,
                    signature: proof.signature,
                    verified_at_ms: 100,
                }
            ),
            Err(nlos_identity::IdentityAuthorityError::InvalidSignature)
        ));
        assert!(
            service
                .mint_reference(&call, &mint_envelope(&call, &request, scope), Some(&proof))
                .is_err()
        );
    }
    let wrong_key = fixture.mint_call(32);
    assert!(
        service
            .mint_reference(
                &wrong_key,
                &mint_envelope(&wrong_key, &request, signed.scope),
                Some(&signed)
            )
            .is_err()
    );
    let wrong_signer = SignedMintCommand {
        signer: fixture.third.principal_id,
        ..signed
    };
    assert!(matches!(
        service.mint_reference(&call, &wire, Some(&wrong_signer)),
        Err(BlobError::BindingMismatch)
    ));
    let wrong_signature = SignedMintCommand {
        signature: [0; 64],
        ..signed
    };
    assert!(
        service
            .mint_reference(&call, &wire, Some(&wrong_signature))
            .is_err()
    );
    assert_eq!(fixture.resolutions.load(Ordering::SeqCst), 0);
}

#[test]
fn mint_requires_a_valid_recipient_and_proof_for_current_process() {
    let fixture = Fixture::new();
    let service = fixture.service(BlobConfig::default()).unwrap();
    let request = fixture.mint_request(b"data", 2, 4);
    let (mut call, _, signed) = fixture.signed_mint(&request, 31);
    call.caller.process_generation = Generation::INITIAL.checked_next().unwrap();
    assert!(matches!(
        service.mint_reference(
            &call,
            &mint_envelope(&call, &request, signed.scope),
            Some(&signed)
        ),
        Err(BlobError::Authority(
            nlos_capability::CapabilityAuthorityError::SignatureInvalid
        ))
    ));
    let mut incorrect = request.clone();
    incorrect.recipient_principal_id.clear();
    assert!(matches!(
        fixture.mint(&service, &incorrect, 33),
        Err(BlobError::InvalidRequest)
    ));
    incorrect.recipient_principal_id = vec![0; 16];
    assert!(matches!(
        fixture.mint(&service, &incorrect, 33),
        Err(BlobError::Identity(
            nlos_identity::IdentityAuthorityError::PrincipalNotFound(_)
        ))
    ));
    assert_eq!(fixture.resolutions.load(Ordering::SeqCst), 0);
    fixture.mint(&service, &request, 31).unwrap();
}

#[test]
fn release_is_signed_for_issuer_or_recipient_and_replay_rechecks_current_policy() {
    let fixture = Fixture::new();
    let service = fixture.service(BlobConfig::default()).unwrap();
    let request = fixture.mint_request(b"data", 2, 3);
    for (index, actor) in [fixture.issuer, fixture.holder].into_iter().enumerate() {
        let key = 31 + u8::try_from(index).unwrap();
        let (_, reference) = fixture.mint(&service, &request, key).unwrap();
        let (third_call, third_wire, third_proof) =
            fixture.signed_release(fixture.third, &reference, 33);
        assert!(matches!(
            service.release_reference(&third_call, &third_wire, Some(&third_proof)),
            Err(BlobError::NotFound)
        ));
        let (call, wire, proof) = fixture.signed_release(actor, &reference, 34);
        assert!(matches!(
            service.release_reference(&call, &wire, None),
            Err(BlobError::MissingSignature)
        ));
        let forged = SignedReleaseCommand {
            signature: [0; 64],
            ..proof
        };
        assert!(
            service
                .release_reference(&call, &wire, Some(&forged))
                .is_err()
        );
        let released = service
            .handle_for_ipc(&call, &wire, Some(BlobCommandProof::Release(&proof)))
            .unwrap();
        assert_eq!(
            service
                .release_reference(&call, &wire, Some(&proof))
                .unwrap(),
            released
        );
        assert!(matches!(
            fixture.read(&service, &reference, 0, 2, 35),
            Err(BlobError::Released)
        ));
        fixture.denied.store(true, Ordering::SeqCst);
        assert!(matches!(
            service.release_reference(&call, &wire, Some(&proof)),
            Err(BlobError::Authorization(AuthorizationError::Denied))
        ));
        fixture.denied.store(false, Ordering::SeqCst);
    }
}

fn revoke_key(fixture: &Fixture, binding: IdentityBinding) {
    fixture
        .identity
        .revoke_key(nlos_identity::RevokeKeyRequest {
            key_id: binding.key_id,
            expected_key_generation: binding.key_generation,
            expected_identity_snapshot_id: binding.identity_snapshot_id,
            idempotency_key: IdempotencyKey::from_bytes(
                [75 + u8::from(binding == fixture.holder); 16],
            ),
            revoked_at_ms: 100,
        })
        .unwrap();
}

#[test]
fn identity_revocation_and_clock_failures_stop_reads_including_exact_replays() {
    let fixture = Fixture::new();
    let service = fixture.service(BlobConfig::default()).unwrap();
    let request = fixture.mint_request(b"data", 2, 3);
    let (_, reference) = fixture.mint(&service, &request, 31).unwrap();
    fixture.read(&service, &reference, 0, 2, 32).unwrap();
    fixture.clock_failed.store(true, Ordering::SeqCst);
    assert!(matches!(
        fixture.read(&service, &reference, 0, 2, 32),
        Err(BlobError::Authorization(
            AuthorizationError::TimeUnavailable
        ))
    ));
    fixture.clock_failed.store(false, Ordering::SeqCst);
    revoke_key(&fixture, fixture.holder);
    assert!(matches!(
        fixture.read(&service, &reference, 0, 2, 32),
        Err(BlobError::Identity(
            nlos_identity::IdentityAuthorityError::KeyRevoked
        ))
    ));
    assert!(fixture.mint(&service, &request, 33).is_err());
    revoke_key(&fixture, fixture.issuer);
    assert!(matches!(
        fixture.mint(&service, &request, 31),
        Err(BlobError::Authority(
            nlos_capability::CapabilityAuthorityError::KeyRevoked
        ))
    ));
    assert_eq!(fixture.reads(), 1);
}

#[test]
fn maximal_chunk_is_bounded_and_canonical_and_eof_does_not_charge() {
    let fixture = Fixture::new();
    let service = fixture.service(BlobConfig::default()).unwrap();
    let request = fixture.mint_request(&vec![42; 65_409], 65_408, 3);
    let (_, reference) = fixture.mint(&service, &request, 31).unwrap();
    let call = fixture.read_call(32);
    let mut read = ReadReferenceRequest {
        schema: Some(blob_reference_schema_identity()),
        reference_id: reference.clone(),
        offset: 0,
        length_bytes: 65_408,
    };
    read.schema.as_mut().unwrap().non_critical_extension_ids = vec![500, 501];
    let wire = envelope(
        &call,
        "read_reference",
        encode_read_reference_request(&read).unwrap(),
        read_input_digest(&read),
    );
    let first = service.handle_for_ipc(&call, &wire, None).unwrap();
    assert!(first.payload.len() <= 65_536);
    let first = decode_read_reference_result(&first.payload).unwrap();
    assert_eq!(first.chunk.len(), 65_408);
    assert_eq!(first.schema.unwrap(), blob_reference_schema_identity());
    let short = fixture.read(&service, &reference, 65_408, 1, 33).unwrap();
    assert_eq!(
        decode_read_reference_result(&short.payload).unwrap().chunk,
        [42]
    );
    for (offset, len) in [(1, 65_408), (0, 65_407), (0, 0), (0, 65_409), (u64::MAX, 2)] {
        assert!(matches!(
            fixture.read(&service, &reference, offset, len, 34),
            Err(BlobError::InvalidRange)
        ));
    }
    assert!(matches!(
        fixture.read(&service, &reference, 65_409, 1, 34),
        Err(BlobError::EndOfFile)
    ));
    let empty = fixture.mint_request(&[], 1, 1);
    let (_, empty) = fixture.mint(&service, &empty, 35).unwrap();
    assert!(matches!(
        fixture.read(&service, &empty, 0, 1, 36),
        Err(BlobError::EndOfFile)
    ));
    assert_eq!(fixture.reads(), 2);
}

#[test]
fn manifest_and_file_bounds_reject_before_resolving_the_path() {
    let fixture = Fixture::new();
    let service = fixture.service(BlobConfig::default()).unwrap();
    let request = fixture.mint_request(&vec![7; 4_097], 1, 5);
    assert!(matches!(
        fixture.mint(&service, &request, 31),
        Err(BlobError::QuotaExceeded)
    ));
    let oversized = MintReferenceRequest {
        size_bytes: 64 * 1024 * 1024 + 1,
        max_chunk_bytes: 65_408,
        ..request.clone()
    };
    assert!(matches!(
        fixture.mint(&service, &oversized, 32),
        Err(BlobError::QuotaExceeded)
    ));
    assert_eq!(fixture.resolutions.load(Ordering::SeqCst), 0);
    let request = fixture.mint_request(&vec![7; 4_096], 1, 5);
    let (_, reference) = fixture.mint(&service, &request, 33).unwrap();
    assert_eq!(
        decode_read_reference_result(
            &fixture
                .read(&service, &reference, 4_095, 1, 34)
                .unwrap()
                .payload
        )
        .unwrap()
        .chunk,
        [7]
    );
}

#[test]
fn pinned_file_survives_path_replacement_and_detects_changes_to_the_original_object() {
    let fixture = Fixture::new();
    let service = fixture.service(BlobConfig::default()).unwrap();
    let request = fixture.mint_request(b"aabbcc", 2, 8);
    let (_, reference) = fixture.mint(&service, &request, 31).unwrap();
    let original = fixture.file().with_file_name("original.wav");
    std::fs::rename(fixture.file(), &original).unwrap();
    let outside = fixture.root.0.join("outside.wav");
    std::fs::write(&outside, b"OUTSID").unwrap();
    std::fs::hard_link(&outside, fixture.file()).unwrap();
    assert_eq!(
        decode_read_reference_result(
            &fixture
                .read(&service, &reference, 0, 2, 32)
                .unwrap()
                .payload
        )
        .unwrap()
        .chunk,
        b"aa"
    );
    std::fs::write(&original, b"aaxxzz").unwrap();
    assert!(matches!(
        fixture.read(&service, &reference, 2, 2, 33),
        Err(BlobError::ContentChanged)
    ));
    std::fs::write(&original, b"aabb").unwrap();
    assert!(matches!(
        fixture.read(&service, &reference, 0, 2, 32),
        Err(BlobError::ContentChanged)
    ));
    std::fs::write(&original, b"aabbccdd").unwrap();
    assert!(matches!(
        fixture.read(&service, &reference, 0, 2, 32),
        Err(BlobError::ContentChanged)
    ));
    assert_eq!(fixture.reads(), 1);
}

#[test]
fn same_key_replay_concurrent_and_reconnect_share_one_receipt_and_authorization() {
    let fixture = Fixture::new();
    let service = fixture.service(BlobConfig::default()).unwrap();
    let request = fixture.mint_request(b"aabbcc", 2, 3);
    let (_, reference) = fixture.mint(&service, &request, 31).unwrap();
    let (first, second) = std::thread::scope(|scope| {
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let invoke = |barrier: Arc<std::sync::Barrier>, session| {
            let mut call = fixture.read_call(40);
            call.caller.session = SessionId::from_bytes([session; 16]);
            let wire = read_envelope(&call, &reference, 0, 2);
            barrier.wait();
            service.read_reference(&call, &wire).unwrap()
        };
        let first_barrier = Arc::clone(&barrier);
        let second_barrier = Arc::clone(&barrier);
        let invoke = &invoke;
        let (first, second) = std::thread::scope(|threads| {
            let first = threads.spawn(move || invoke(first_barrier, 1));
            let second = threads.spawn(move || invoke(second_barrier, 2));
            barrier.wait();
            (first.join().unwrap(), second.join().unwrap())
        });
        let _ = scope;
        (first, second)
    });
    assert_eq!(first, second);
    assert_eq!(fixture.reads(), 1);
    assert!(matches!(
        fixture.read(&service, &reference, 2, 2, 40),
        Err(BlobError::IdempotencyConflict)
    ));
    fixture.denied.store(true, Ordering::SeqCst);
    assert!(matches!(
        fixture.read(&service, &reference, 0, 2, 40),
        Err(BlobError::Authorization(AuthorizationError::Denied))
    ));
    assert_eq!(fixture.reads(), 1);
}

#[test]
fn max_reads_linearizes_concurrent_distinct_keys_and_closes_the_pin() {
    let fixture = Fixture::new();
    let service = fixture
        .service(BlobConfig {
            active_global: 1,
            ..BlobConfig::default()
        })
        .unwrap();
    let request = fixture.mint_request(b"aabbcc", 2, 1);
    let (mint, reference) = fixture.mint(&service, &request, 31).unwrap();
    assert_eq!(fixture.mint(&service, &request, 31).unwrap().0, mint);
    assert!(matches!(
        fixture.mint(&service, &request, 32),
        Err(BlobError::QuotaExceeded)
    ));
    std::thread::scope(|scope| {
        let barrier = std::sync::Barrier::new(3);
        let read = |key| {
            barrier.wait();
            fixture.read(&service, &reference, 0, 2, key)
        };
        std::thread::scope(|threads| {
            let first = threads.spawn(|| read(36));
            let second = threads.spawn(|| read(37));
            barrier.wait();
            let results = [first.join().unwrap(), second.join().unwrap()];
            assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
            assert!(
                results
                    .iter()
                    .any(|result| matches!(result, Err(BlobError::Exhausted)))
            );
        });
        let _ = scope;
    });
    assert_eq!(fixture.reads(), 1);
    assert_ne!(fixture.mint(&service, &request, 32).unwrap().1, reference);
}

fn pause_read_policy(
    fixture: &Fixture,
    phase: usize,
) -> (
    std::sync::mpsc::Receiver<()>,
    std::sync::mpsc::SyncSender<()>,
) {
    let (entered, reached) = std::sync::mpsc::sync_channel(1);
    let (resume, resume_signal) = std::sync::mpsc::sync_channel(1);
    let resume_signal = Mutex::new(resume_signal);
    let calls = AtomicUsize::new(0);
    *fixture.hook.lock().unwrap() = Some(Arc::new(move |binding| {
        if binding.action.method == ServiceMethod::ReadReference
            && calls.fetch_add(1, Ordering::SeqCst) == phase
        {
            entered.send(()).unwrap();
            resume_signal
                .lock()
                .unwrap()
                .recv_timeout(std::time::Duration::from_secs(3))
                .unwrap();
        }
    }));
    (reached, resume)
}

fn release(fixture: &Fixture, service: &BlobReferenceService, reference: &[u8], marker: u8) {
    let (call, wire, proof) = fixture.signed_release(fixture.issuer, reference, marker);
    service
        .release_reference(&call, &wire, Some(&proof))
        .unwrap();
}

#[test]
fn release_between_io_and_receipt_blocks_the_chunk_and_future_replay() {
    let fixture = Fixture::new();
    let service = fixture.service(BlobConfig::default()).unwrap();
    let request = fixture.mint_request(b"aabb", 2, 3);
    let (_, reference) = fixture.mint(&service, &request, 31).unwrap();
    fixture.read(&service, &reference, 0, 2, 32).unwrap();
    let (reached, resume) = pause_read_policy(&fixture, 1);
    std::thread::scope(|scope| {
        let reader = scope.spawn(|| fixture.read(&service, &reference, 2, 2, 33));
        reached
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap();
        release(&fixture, &service, &reference, 34);
        resume.send(()).unwrap();
        assert!(matches!(reader.join().unwrap(), Err(BlobError::Released)));
    });
    assert!(matches!(
        fixture.read(&service, &reference, 0, 2, 32),
        Err(BlobError::Released)
    ));
    assert_eq!(fixture.reads(), 1);
}

#[test]
fn expiry_between_io_and_receipt_closes_the_pin_on_maintenance() {
    let fixture = Fixture::new();
    let service = fixture
        .service(BlobConfig {
            active_per_principal: 1,
            ..BlobConfig::default()
        })
        .unwrap();
    let mut request = fixture.mint_request(b"aabb", 2, 3);
    request.valid_until_ms = 110;
    let (_, reference) = fixture.mint(&service, &request, 31).unwrap();
    let (reached, resume) = pause_read_policy(&fixture, 1);
    std::thread::scope(|scope| {
        let reader = scope.spawn(|| fixture.read(&service, &reference, 0, 2, 32));
        reached
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap();
        fixture.clock.store(110, Ordering::SeqCst);
        assert_eq!(service.maintain().unwrap(), 1);
        resume.send(()).unwrap();
        assert!(matches!(reader.join().unwrap(), Err(BlobError::Expired)));
    });
    assert_eq!(fixture.reads(), 0);
    request.valid_until_ms = 200;
    fixture.mint(&service, &request, 33).unwrap();
}

#[test]
fn quota_and_rate_limits_apply_across_new_sessions() {
    let fixture = Fixture::new();
    let service = fixture
        .service(BlobConfig {
            active_per_principal: 1,
            reads_per_principal: 1,
            reads_global: 1,
            mints_per_principal: 1,
            ..BlobConfig::default()
        })
        .unwrap();
    let request = fixture.mint_request(b"aabb", 2, 2);
    let (_, reference) = fixture.mint(&service, &request, 31).unwrap();
    let (reached, resume) = pause_read_policy(&fixture, 0);
    std::thread::scope(|scope| {
        let reader = scope.spawn(|| fixture.read(&service, &reference, 0, 2, 32));
        reached
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap();
        let mut alternate = fixture.read_call(33);
        alternate.caller.session = SessionId::from_bytes([101; 16]);
        assert!(matches!(
            service.read_reference(&alternate, &read_envelope(&alternate, &reference, 2, 2)),
            Err(BlobError::QuotaExceeded)
        ));
        resume.send(()).unwrap();
        reader.join().unwrap().unwrap();
    });
    release(&fixture, &service, &reference, 34);
    let (mut mint_call, mint_wire, signed) = fixture.signed_mint(&request, 35);
    mint_call.caller.session = SessionId::from_bytes([102; 16]);
    assert!(matches!(
        service.mint_reference(&mint_call, &mint_wire, Some(&signed)),
        Err(BlobError::RateLimited)
    ));
    fixture.clock.store(60_100, Ordering::SeqCst);
    let mut next = request;
    next.valid_until_ms = 61_000;
    fixture.mint(&service, &next, 36).unwrap();
}

#[test]
fn restart_fences_old_tokens_and_replays_mint_without_even_resolving_path() {
    let fixture = Fixture::new();
    let service = fixture.service(BlobConfig::default()).unwrap();
    let request = fixture.mint_request(b"aabb", 2, 3);
    let (reply, reference) = fixture.mint(&service, &request, 31).unwrap();
    let (other_reply, other_reference) = fixture.mint(&service, &request, 35).unwrap();
    release(&fixture, &service, &other_reference, 36);
    fixture.read(&service, &reference, 0, 2, 32).unwrap();
    let reopened = Arc::new(CapabilityAuthority::open(fixture.root.0.join("capability")).unwrap());
    assert!(matches!(
        fixture.service_with_authority(Arc::clone(&reopened), BlobConfig::default()),
        Err(BlobError::InstanceActive)
    ));
    let resolves = fixture.resolutions.load(Ordering::SeqCst);
    drop(service);
    std::fs::remove_file(fixture.file()).unwrap();
    let restarted = fixture
        .service_with_authority(reopened, BlobConfig::default())
        .unwrap();
    assert!(matches!(
        fixture.read(&restarted, &reference, 0, 2, 32),
        Err(BlobError::NotFound)
    ));
    assert_eq!(fixture.mint(&restarted, &request, 31).unwrap().0, reply);
    let altered = MintReferenceRequest {
        max_reads: 2,
        ..request.clone()
    };
    assert!(matches!(
        fixture.mint(&restarted, &altered, 31),
        Err(BlobError::IdempotencyConflict)
    ));
    assert_eq!(
        fixture.mint(&restarted, &request, 35).unwrap().0,
        other_reply
    );
    let (call, wire, proof) = fixture.signed_release(fixture.issuer, &other_reference, 36);
    assert!(
        restarted
            .release_reference(&call, &wire, Some(&proof))
            .is_ok()
    );
    assert_eq!(fixture.resolutions.load(Ordering::SeqCst), resolves);
    assert_eq!(fixture.reads(), 1);
    std::fs::write(fixture.file(), b"aabb").unwrap();
    assert_ne!(fixture.mint(&restarted, &request, 33).unwrap().1, reference);
}

#[test]
fn audit_reservation_blocks_new_mint_but_covers_admitted_reads_and_release() {
    let fixture = Fixture::new();
    let service = fixture
        .service(BlobConfig {
            audit_rows: 4,
            ..BlobConfig::default()
        })
        .unwrap();
    let request = fixture.mint_request(b"aabb", 2, 2);
    let (_, reference) = fixture.mint(&service, &request, 31).unwrap();
    assert!(matches!(
        fixture.mint(&service, &request, 32),
        Err(BlobError::AuditFull)
    ));
    fixture.read(&service, &reference, 0, 2, 33).unwrap();
    fixture.read(&service, &reference, 2, 2, 34).unwrap();
    release(&fixture, &service, &reference, 35);
    assert_eq!(
        fixture
            .db()
            .query_row("SELECT rows FROM blob_audit_usage", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        4
    );
    assert!(matches!(
        fixture.mint(&service, &request, 36),
        Err(BlobError::AuditFull)
    ));
    drop(service);
    let bytes_limited = fixture
        .service(BlobConfig {
            audit_bytes: 65_536,
            ..BlobConfig::default()
        })
        .unwrap();
    assert!(matches!(
        fixture.mint(&bytes_limited, &request, 37),
        Err(BlobError::AuditFull)
    ));
}

#[test]
fn blob_sql_ledger_blocks_immutable_row_edits_replace_and_state_rollback() {
    let fixture = Fixture::new();
    let service = fixture.service(BlobConfig::default()).unwrap();
    let request = fixture.mint_request(b"aabb", 2, 3);
    let (_, reference) = fixture.mint(&service, &request, 31).unwrap();
    fixture.read(&service, &reference, 0, 2, 32).unwrap();
    release(&fixture, &service, &reference, 33);
    let db = fixture.db();
    for statement in [
        "UPDATE blob_tokens SET manifest=zeroblob(length(manifest))",
        "UPDATE blob_tokens SET holder=zeroblob(16)",
        "DELETE FROM blob_tokens",
        "INSERT OR REPLACE INTO blob_tokens SELECT * FROM blob_tokens",
        "UPDATE blob_read_receipts SET ordinal=2",
        "DELETE FROM blob_read_receipts",
        "INSERT OR REPLACE INTO blob_read_receipts SELECT * FROM blob_read_receipts",
        "UPDATE blob_revocations SET released_at_ms=0",
        "DELETE FROM blob_revocations",
        "INSERT OR REPLACE INTO blob_revocations SELECT * FROM blob_revocations",
        "UPDATE blob_boot SET epoch=0",
        "INSERT OR REPLACE INTO blob_boot VALUES(1,0)",
        "UPDATE blob_audit_usage SET rows=0, estimated_bytes=0",
        "INSERT OR REPLACE INTO blob_audit_usage VALUES (1,0,0)",
    ] {
        assert!(db.execute(statement, []).is_err(), "accepted: {statement}");
    }
    assert_eq!(fixture.reads(), 1);
    assert!(matches!(
        fixture.read(&service, &reference, 0, 2, 32),
        Err(BlobError::Released)
    ));
}

#[test]
fn manifest_corruption_cannot_turn_into_a_read_or_charge() {
    let fixture = Fixture::new();
    let service = fixture.service(BlobConfig::default()).unwrap();
    let request = fixture.mint_request(b"aabb", 2, 3);
    let (_, reference) = fixture.mint(&service, &request, 31).unwrap();
    fixture
        .db()
        .execute_batch(
            "DROP TRIGGER blob_tokens_immutable_update;
         UPDATE blob_tokens SET manifest=zeroblob(length(manifest));",
        )
        .unwrap();
    assert!(matches!(
        fixture.read(&service, &reference, 0, 2, 32),
        Err(BlobError::CorruptRecord)
    ));
    assert_eq!(fixture.reads(), 0);
}

#[test]
fn v3_schema_migration_is_atomic_on_schema_error_and_preserves_prior_history() {
    use nlos_store_fault::{FaultCode, FaultMode};
    let _serialization = FAULT_LOCK.lock().unwrap();
    nlos_store_fault::register(FAULT_VFS).unwrap();
    let fixture = Fixture::new();
    let db = fixture.db();
    db.execute_batch(
        "DROP TABLE blob_read_receipts;
         DROP TABLE blob_revocations;
         DROP TABLE blob_tokens;
         DROP TABLE blob_audit_usage;
         DROP TABLE blob_boot;
         CREATE TABLE blob_tokens (unrelated INTEGER);
         PRAGMA user_version=2;",
    )
    .unwrap();
    let root = fixture.root.0.join("capability");
    assert!(CapabilityAuthority::open(&root).is_err());
    assert_eq!(
        db.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name='blob_boot'",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    db.execute("DROP TABLE blob_tokens", []).unwrap();
    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::IoErr,
    });
    let interrupted = CapabilityAuthority::open(fault_uri(&root, FAULT_VFS));
    let writes = nlos_store_fault::writes_observed();
    nlos_store_fault::disarm();
    assert!(writes > 0);
    assert!(matches!(
        interrupted,
        Err(nlos_capability::CapabilityAuthorityError::Sqlite(_))
    ));
    assert_eq!(
        db.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name='blob_tokens'",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    let recovered = CapabilityAuthority::open(&root).unwrap();
    assert_eq!(
        recovered
            .inspect_active(fixture.source, 100)
            .unwrap()
            .holder,
        fixture.issuer.principal_id
    );
    assert_eq!(
        db.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .unwrap(),
        3
    );
}

fn verify_interrupted_epoch(fixture: &Fixture) {
    nlos_store_fault::arm(nlos_store_fault::FaultMode::FailWritesAfter {
        remaining: 0,
        code: nlos_store_fault::FaultCode::Full,
    });
    let interrupted = fixture.service(BlobConfig::default());
    let writes = nlos_store_fault::writes_observed();
    nlos_store_fault::disarm();
    assert!(writes > 0);
    assert!(matches!(
        interrupted,
        Err(BlobError::Authority(
            nlos_capability::CapabilityAuthorityError::Sqlite(_)
        ))
    ));
    assert_eq!(
        fixture
            .db()
            .query_row("SELECT epoch FROM blob_boot", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn io_faults_roll_back_mint_read_and_release_and_recover_exact_idempotency() {
    use nlos_store_fault::{FaultCode, FaultMode};
    let _serialization = FAULT_LOCK.lock().unwrap();
    nlos_store_fault::register(FAULT_VFS).unwrap();
    let fixture = Fixture::with_authority(|directory| {
        CapabilityAuthority::open(fault_uri(directory, FAULT_VFS)).unwrap()
    });
    let arm = |code| nlos_store_fault::arm(FaultMode::FailWritesAfter { remaining: 0, code });
    verify_interrupted_epoch(&fixture);
    let service = fixture
        .service(BlobConfig {
            active_global: 1,
            ..BlobConfig::default()
        })
        .unwrap();
    let request = fixture.mint_request(b"aabb", 2, 3);
    arm(FaultCode::IoErr);
    let failed_mint = fixture.mint(&service, &request, 31);
    nlos_store_fault::disarm();
    assert!(matches!(
        failed_mint,
        Err(BlobError::Authority(
            nlos_capability::CapabilityAuthorityError::Sqlite(_)
        ))
    ));
    assert_eq!(
        fixture
            .db()
            .query_row("SELECT count(*) FROM blob_tokens", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    let (mint, reference) = fixture.mint(&service, &request, 31).unwrap();
    arm(FaultCode::Full);
    let failed_read = fixture.read(&service, &reference, 0, 2, 32);
    nlos_store_fault::disarm();
    assert!(matches!(
        failed_read,
        Err(BlobError::Authority(
            nlos_capability::CapabilityAuthorityError::Sqlite(_)
        ))
    ));
    assert_eq!(fixture.reads(), 0);
    let read = fixture.read(&service, &reference, 0, 2, 32).unwrap();
    assert_eq!(fixture.read(&service, &reference, 0, 2, 32).unwrap(), read);
    let (call, release, proof) = fixture.signed_release(fixture.issuer, &reference, 33);
    arm(FaultCode::IoErr);
    let failed_release = service.release_reference(&call, &release, Some(&proof));
    nlos_store_fault::disarm();
    assert!(matches!(
        failed_release,
        Err(BlobError::Authority(
            nlos_capability::CapabilityAuthorityError::Sqlite(_)
        ))
    ));
    assert_eq!(
        fixture
            .db()
            .query_row("SELECT count(*) FROM blob_revocations", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    let receipt = service
        .release_reference(&call, &release, Some(&proof))
        .unwrap();
    drop(service);
    let restarted = fixture.service(BlobConfig::default()).unwrap();
    assert_eq!(
        restarted
            .release_reference(&call, &release, Some(&proof))
            .unwrap(),
        receipt
    );
    assert_eq!(fixture.mint(&restarted, &request, 31).unwrap().0, mint);
    assert!(fixture.read(&restarted, &reference, 0, 2, 32).is_err());
    assert_eq!(fixture.reads(), 1);
    assert_eq!(
        fixture
            .db()
            .pragma_query_value(None, "integrity_check", |row| row.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
}

#[test]
fn ancestor_revocation_after_file_io_prevents_read_receipt() {
    use nlos_capability::{
        DelegateCapabilityRequest, RevokeCapabilityRequest, SignedDelegateCapabilityRequest,
        SignedRevokeCapabilityRequest, delegate_command_message, revoke_command_message,
    };
    let mut fixture = Fixture::new();
    let parent = fixture.third_handle;
    let delegate = DelegateCapabilityRequest {
        parent,
        delegator_key_id: fixture.third.key_id,
        recipient_key_id: fixture.issuer.key_id,
        target: CapabilityTarget::Namespace(WORKSPACE.namespace),
        rights: CapabilityRights::DELEGATE,
        purpose_digest: None,
        valid_from_ms: 1,
        valid_until_ms: 5_000,
        delegation_depth_remaining: 0,
        call_limit: None,
        idempotency_key: IdempotencyKey::from_bytes([79; 16]),
        delegated_at_ms: 2,
    };
    let new_source = fixture
        .authority
        .delegate_signed(
            &fixture.identity,
            SignedDelegateCapabilityRequest {
                command: delegate,
                signer: fixture.third.principal_id,
                signature: fixture
                    .third_key
                    .sign(&delegate_command_message(delegate))
                    .to_bytes(),
            },
        )
        .unwrap()
        .receipt();
    fixture.source = CapabilityHandle {
        capability_id: new_source.capability_id,
        generation: new_source.generation,
    };
    let service = fixture.service(BlobConfig::default()).unwrap();
    let mut request = fixture.mint_request(b"aabb", 2, 3);
    request.valid_until_ms = 5_001;
    assert!(matches!(
        fixture.mint(&service, &request, 31),
        Err(BlobError::InvalidRequest)
    ));
    request.valid_until_ms = 4_000;
    let (_, reference) = fixture.mint(&service, &request, 31).unwrap();
    let (reached, resume) = pause_read_policy(&fixture, 1);
    std::thread::scope(|scope| {
        let reader = scope.spawn(|| fixture.read(&service, &reference, 0, 2, 32));
        reached
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap();
        let revoke = RevokeCapabilityRequest {
            handle: parent,
            revoker_key_id: fixture.third.key_id,
            idempotency_key: IdempotencyKey::from_bytes([80; 16]),
            revoked_at_ms: 100,
        };
        fixture
            .authority
            .revoke_signed(
                &fixture.identity,
                SignedRevokeCapabilityRequest {
                    command: revoke,
                    signer: fixture.third.principal_id,
                    signature: fixture
                        .third_key
                        .sign(&revoke_command_message(revoke))
                        .to_bytes(),
                },
            )
            .unwrap();
        resume.send(()).unwrap();
        assert!(matches!(
            reader.join().unwrap(),
            Err(BlobError::Authority(
                nlos_capability::CapabilityAuthorityError::AncestorRevokedOrFenced
            ))
        ));
    });
    assert_eq!(fixture.reads(), 0);
}

#[test]
fn writer_lease_is_exclusive_across_processes() {
    let fixture = Fixture::new();
    let _service = fixture.service(BlobConfig::default()).unwrap();
    let probe = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "writer_lease_child_probe"])
        .env("NLOS_TEST_BLOB_LEASE_DIR", &fixture.root.0)
        .output()
        .unwrap();
    assert!(
        probe.status.success(),
        "writer lease child failed: {}",
        String::from_utf8_lossy(&probe.stdout)
    );
}

#[test]
fn writer_lease_child_probe() {
    let Some(root) = std::env::var_os("NLOS_TEST_BLOB_LEASE_DIR") else {
        return;
    };
    let root = std::path::PathBuf::from(root);
    let authority = Arc::new(CapabilityAuthority::open(root.join("capability")).unwrap());
    let identity = Arc::new(IdentityAuthority::open(root.join("identity")).unwrap());
    let resolver = Arc::new(LocalWorkspace {
        issuers: [PrincipalId::from_bytes([0; 16]); 2],
        directory: Dir::open_ambient_dir(root.join("workspace"), ambient_authority()).unwrap(),
        resolutions: Arc::new(AtomicUsize::new(0)),
    });
    assert!(matches!(
        BlobReferenceService::new(
            authority,
            identity,
            Arc::new(nlos_capability::service_call::DenyAllAuthorizer),
            resolver,
            RootedWorkspaceOpener,
            Arc::new(|| Err(AuthorizationError::TimeUnavailable)),
            BlobConfig::default(),
        ),
        Err(BlobError::InstanceActive)
    ));
}

#[test]
#[ignore = "run in isolation to measure process memory under the maximum file and block sizes"]
fn bounded_blob_buffers_peak_probe() {
    use std::io::Write as _;
    const EXTENT: u64 = 64 * 1024 * 1024;
    let fixture = Fixture::new();
    let service = fixture.service(BlobConfig::default()).unwrap();
    let mut request = fixture.mint_request(&[], 16_384, 4);
    let mut file = std::fs::File::create(fixture.file()).unwrap();
    let page = [0; 16_384];
    let mut digest = Sha256::new();
    for _ in 0..4_096 {
        file.write_all(&page).unwrap();
        digest.update(page);
    }
    drop(file);
    request.size_bytes = EXTENT;
    request.file_digest_sha256 = digest.finalize().to_vec();
    let (_, manifest_limit) = fixture.mint(&service, &request, 31).unwrap();
    assert_eq!(
        fixture
            .db()
            .query_row(
                "SELECT length(manifest) FROM blob_tokens WHERE reference_id=?1",
                [&manifest_limit],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        128 * 1024
    );
    request.max_chunk_bytes = 65_408;
    let mut references = Vec::new();
    for key in 32..36 {
        references.push(fixture.mint(&service, &request, key).unwrap().1);
    }
    let count = AtomicUsize::new(0);
    *fixture.hook.lock().unwrap() = Some(Arc::new(move |binding| {
        if binding.action.method == ServiceMethod::ReadReference {
            let phase = count.fetch_add(1, Ordering::SeqCst) / 4;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            while count.load(Ordering::SeqCst) / 4 <= phase {
                assert!(
                    std::time::Instant::now() < deadline,
                    "read probe rendezvous timed out"
                );
                std::thread::yield_now();
            }
        }
    }));
    std::thread::scope(|scope| {
        let reads: Vec<_> = references
            .iter()
            .enumerate()
            .map(|(index, reference)| {
                let fixture = &fixture;
                let service = &service;
                scope.spawn(move || {
                    fixture
                        .read(
                            service,
                            reference,
                            0,
                            65_408,
                            40 + u8::try_from(index).unwrap(),
                        )
                        .unwrap()
                })
            })
            .collect();
        for read in reads {
            let reply = read.join().unwrap();
            assert!(reply.payload.len() <= 65_536);
            assert_eq!(
                decode_read_reference_result(&reply.payload)
                    .unwrap()
                    .chunk
                    .len(),
                65_408
            );
        }
    });
    assert_eq!(fixture.reads(), 4);
}

#[test]
fn audit_bytes_cover_the_full_admitted_sqlite_read_and_release_tail() {
    let fixture = Fixture::new();
    let limit = 5_000_000;
    let service = fixture
        .service(BlobConfig {
            audit_bytes: limit,
            ..BlobConfig::default()
        })
        .unwrap();
    let request = fixture.mint_request(b"a", 1, 4096);
    let reference = match fixture.mint(&service, &request, 31) {
        Err(BlobError::AuditFull) => {
            assert_eq!(fixture.resolutions.load(Ordering::SeqCst), 0);
            return;
        }
        Ok((_, reference)) => reference,
        Err(error) => panic!("unexpected audit admission result: {error}"),
    };
    complete_audit_tail(&fixture, &service, &reference, 4096, limit);
}

fn complete_audit_tail(
    fixture: &Fixture,
    service: &BlobReferenceService,
    reference: &[u8],
    reads: u64,
    limit: u64,
) {
    for ordinal in 1..=reads {
        let mut call = fixture.read_call(40);
        let mut key = [40_u8; 16];
        key[..8].copy_from_slice(&ordinal.to_be_bytes());
        call.idempotency_key = Some(IdempotencyKey::from_bytes(key));
        let wire = read_envelope(&call, reference, 0, 1);
        if let Err(error) = service.read_reference(&call, &wire) {
            let (release_call, release_wire, proof) =
                fixture.signed_release(fixture.issuer, reference, 250);
            let released = service.release_reference(&release_call, &release_wire, Some(&proof));
            panic!("admitted budget stranded at read {ordinal}: {error:?}; release={released:?}");
        }
    }
    release(fixture, service, reference, 250);
    assert_eq!(fixture.reads(), i64::try_from(reads).unwrap());
    let db = fixture.root.0.join("capability/capability-authority.db");
    let wal = db.with_file_name("capability-authority.db-wal");
    let observed =
        std::fs::metadata(db).unwrap().len() + std::fs::metadata(wal).map_or(0, |item| item.len());
    eprintln!(
        "audit tail: {reads} durable reads and release; DB+WAL={observed} bytes, cap={limit}"
    );
    assert!(observed <= limit);
}

#[test]
fn short_deadline_bounds_wait_for_the_descriptor_mutex() {
    use std::time::{Duration, Instant};
    let fixture = Fixture::new();
    let service = fixture.service(BlobConfig::default()).unwrap();
    let request = fixture.mint_request(b"aabb", 2, 3);
    let (_, reference) = fixture.mint(&service, &request, 31).unwrap();
    let (reached, resume) = pause_read_policy(&fixture, 0);
    let service = &service;
    std::thread::scope(|scope| {
        let first = scope.spawn(|| fixture.read(service, &reference, 0, 2, 32));
        reached.recv_timeout(Duration::from_secs(3)).unwrap();
        let mut call = fixture.read_call(33);
        call.deadline_monotonic_ns = Some(120_000_000);
        let wire = read_envelope(&call, &reference, 2, 2);
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let second = scope.spawn(move || {
            let started = Instant::now();
            started_tx.send(()).unwrap();
            let result = service.read_reference(&call, &wire);
            (started.elapsed(), result)
        });
        started_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        std::thread::sleep(Duration::from_millis(250));
        resume.send(()).unwrap();
        first.join().unwrap().unwrap();
        let (elapsed, result) = second.join().unwrap();
        eprintln!("fd contention: deadline=20ms elapsed={elapsed:?} result={result:?}");
        assert!(matches!(
            result,
            Err(BlobError::Authorization(
                AuthorizationError::DeadlineExpired
            ))
        ));
        assert!(elapsed < Duration::from_millis(200));
    });
    assert_eq!(fixture.reads(), 1);
}

#[test]
fn short_deadline_bounds_wait_for_the_sqlite_writer() {
    use std::time::{Duration, Instant};
    let fixture = Fixture::new();
    let service = fixture.service(BlobConfig::default()).unwrap();
    let request = fixture.mint_request(b"aabb", 2, 3);
    let (_, reference) = fixture.mint(&service, &request, 31).unwrap();
    let blocking = fixture.db();
    blocking.execute_batch("BEGIN IMMEDIATE").unwrap();
    let (mut call, _, proof) = fixture.signed_release(fixture.issuer, &reference, 32);
    call.deadline_monotonic_ns = Some(120_000_000);
    let request = ReleaseReferenceRequest {
        schema: Some(blob_reference_schema_identity()),
        reference_id: reference.clone(),
        command_proof: None,
    };
    let wire = envelope(
        &call,
        "release_reference",
        encode_release_reference_request(&request).unwrap(),
        release_input_digest(&request, proof.scope),
    );
    let service = &service;
    std::thread::scope(|scope| {
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let operation = scope.spawn(move || {
            let started = Instant::now();
            started_tx.send(()).unwrap();
            let result = service.release_reference(&call, &wire, Some(&proof));
            (started.elapsed(), result)
        });
        started_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        std::thread::sleep(Duration::from_millis(250));
        blocking.execute_batch("ROLLBACK").unwrap();
        let (elapsed, result) = operation.join().unwrap();
        eprintln!("SQLite contention: deadline=20ms elapsed={elapsed:?} result={result:?}");
        assert!(matches!(
            result,
            Err(BlobError::Authorization(
                AuthorizationError::DeadlineExpired
            ))
        ));
        assert!(elapsed < Duration::from_millis(200));
    });
    assert_eq!(
        fixture
            .db()
            .query_row("SELECT count(*) FROM blob_revocations", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    fixture.read(service, &reference, 0, 2, 33).unwrap();
}

#[test]
fn audit_checkpoint_and_terminal_reserve_support_the_maximum_paid_reads() {
    let fixture = Fixture::new();
    let config = BlobConfig::default();
    let service = fixture.service(config).unwrap();
    let request = fixture.mint_request(b"a", 1, config.max_reads);
    let (_, reference) = fixture.mint(&service, &request, 31).unwrap();
    complete_audit_tail(
        &fixture,
        &service,
        &reference,
        config.max_reads,
        config.audit_bytes,
    );
}

#[test]
fn physical_audit_bound_uses_the_database_page_size() {
    let fixture = Fixture::with_authority(|root| {
        let db = rusqlite::Connection::open(root.join("capability-authority.db")).unwrap();
        db.pragma_update(None, "page_size", 1024).unwrap();
        db.execute_batch("VACUUM").unwrap();
        drop(db);
        CapabilityAuthority::open(root).unwrap()
    });
    let size: i64 = fixture
        .db()
        .pragma_query_value(None, "page_size", |row| row.get(0))
        .unwrap();
    assert_eq!(size, 1024);
    let config = BlobConfig {
        audit_bytes: 3_000_000,
        ..BlobConfig::default()
    };
    let service = fixture.service(config).unwrap();
    let request = fixture.mint_request(b"a", 1, 70);
    let (_, reference) = fixture.mint(&service, &request, 31).unwrap();
    assert!(matches!(
        fixture.mint_as_third(&service, &request, 31),
        Err(BlobError::AuditFull)
    ));
    complete_audit_tail(&fixture, &service, &reference, 70, config.audit_bytes);
}

#[test]
fn blocked_checkpoint_expires_without_charging_and_recovers_when_reader_leaves() {
    use std::time::{Duration, Instant};

    let _fault = FAULT_LOCK.lock().unwrap();
    nlos_store_fault::register(FAULT_VFS).unwrap();
    let fixture = Fixture::with_authority(|directory| {
        CapabilityAuthority::open(fault_uri(directory, FAULT_VFS)).unwrap()
    });
    let service = fixture.service(BlobConfig::default()).unwrap();
    let request = fixture.mint_request(b"a", 1, 64);
    let (_, reference) = fixture.mint(&service, &request, 31).unwrap();
    let wal = fixture
        .db_path()
        .with_file_name("capability-authority.db-wal");
    let observer = fixture.db();
    observer.busy_timeout(Duration::ZERO).unwrap();
    let page_size: u64 = u64::try_from(
        observer
            .pragma_query_value(None, "page_size", |row| row.get::<_, i64>(0))
            .unwrap(),
    )
    .unwrap();
    let window = 32 + 64 * (page_size + 24);
    for key in 40..90 {
        if std::fs::metadata(&wal).unwrap().len() > window {
            break;
        }
        fixture.read(&service, &reference, 0, 1, key).unwrap();
    }
    assert!(std::fs::metadata(&wal).unwrap().len() > window);
    let snapshot = fixture.db();
    snapshot.execute_batch("BEGIN").unwrap();
    let reads_before: i64 = snapshot
        .query_row("SELECT count(*) FROM blob_read_receipts", [], |row| {
            row.get(0)
        })
        .unwrap();

    assert_checkpoint_backfill_attempted(&fixture, &service, &reference);
    let (busy, frames, backfilled): (i64, i64, i64) = observer
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .unwrap();
    assert_eq!(busy, 1);
    assert!(frames > 0);
    assert_eq!(backfilled, frames);
    assert!(std::fs::metadata(&wal).unwrap().len() > window);

    let mut call = fixture.read_call(100);
    call.deadline_monotonic_ns = Some(120_000_000);
    let wire = read_envelope(&call, &reference, 0, 1);
    let started = Instant::now();
    let result = service.read_reference(&call, &wire);
    eprintln!("blocked checkpoint read elapsed {:?}", started.elapsed());
    assert!(matches!(
        result,
        Err(BlobError::Authorization(
            AuthorizationError::DeadlineExpired
        ))
    ));
    assert_eq!(fixture.reads(), reads_before);
    snapshot.execute_batch("ROLLBACK").unwrap();
    fixture.read(&service, &reference, 0, 1, 100).unwrap();
    assert_eq!(fixture.reads(), reads_before + 1);
    release(&fixture, &service, &reference, 101);
}

fn assert_checkpoint_backfill_attempted(
    fixture: &Fixture,
    service: &BlobReferenceService,
    reference: &[u8],
) {
    use nlos_store_fault::{FaultCode, FaultMode};

    // The held snapshot blocks truncation. The first write is checkpoint backfill.
    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::IoErr,
    });
    let result = fixture.read(service, reference, 0, 1, 100);
    let writes = nlos_store_fault::writes_observed();
    nlos_store_fault::disarm();
    assert!(writes > 0);
    assert!(matches!(
        result,
        Err(BlobError::Authority(nlos_capability::CapabilityAuthorityError::Sqlite(
            rusqlite::Error::SqliteFailure(error, _)
        ))) if error.code == rusqlite::ErrorCode::SystemIoFailure
    ));
}

#[test]
fn independently_enforces_per_recipient_read_concurrency() {
    assert_concurrent_read_limits(1, 2, true);
}

#[test]
fn independently_enforces_global_read_concurrency_across_recipients() {
    assert_concurrent_read_limits(4, 1, false);
}

fn assert_concurrent_read_limits(per_principal: usize, global: usize, separate_allowed: bool) {
    let fixture = Fixture::new();
    let service = fixture
        .service(BlobConfig {
            reads_per_principal: per_principal,
            reads_global: global,
            ..BlobConfig::default()
        })
        .unwrap();
    let request = fixture.mint_request(b"aabb", 2, 3);
    let (_, held_reference) = fixture.mint(&service, &request, 31).unwrap();
    let other_request = MintReferenceRequest {
        recipient_principal_id: fixture.issuer.principal_id.as_bytes().to_vec(),
        ..request
    };
    let other_reference = fixture.mint_as_third(&service, &other_request, 32).unwrap();
    let (reached, resume) = pause_read_policy(&fixture, 0);
    std::thread::scope(|scope| {
        let in_flight = scope.spawn(|| fixture.read(&service, &held_reference, 0, 2, 40));
        reached
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap();
        if separate_allowed {
            assert!(matches!(
                fixture.read(&service, &held_reference, 2, 2, 41),
                Err(BlobError::QuotaExceeded)
            ));
        }
        let call = fixture.mint_call(40);
        let wire = read_envelope(&call, &other_reference, 0, 2);
        let result = service.read_reference(&call, &wire);
        if separate_allowed {
            assert!(result.is_ok(), "{result:?}");
        } else {
            assert!(matches!(result, Err(BlobError::QuotaExceeded)));
        }
        resume.send(()).unwrap();
        in_flight.join().unwrap().unwrap();
    });
}

#[test]
fn global_mint_rate_counts_different_issuers() {
    let fixture = Fixture::new();
    let service = fixture
        .service(BlobConfig {
            mints_global: 1,
            ..BlobConfig::default()
        })
        .unwrap();
    let mut request = fixture.mint_request(b"a", 1, 1);
    fixture.mint(&service, &request, 31).unwrap();
    assert!(matches!(
        fixture.mint_as_third(&service, &request, 31),
        Err(BlobError::RateLimited)
    ));
    fixture.clock.store(60_100, Ordering::SeqCst);
    request.valid_until_ms = 61_000;
    fixture.mint_as_third(&service, &request, 31).unwrap();
}

#[test]
fn recipient_pin_quota_counts_distinct_issuers() {
    let fixture = Fixture::new();
    let service = fixture
        .service(BlobConfig {
            active_per_principal: 1,
            ..BlobConfig::default()
        })
        .unwrap();
    let request = fixture.mint_request(b"a", 1, 1);
    let (_, reference) = fixture.mint(&service, &request, 31).unwrap();
    assert!(matches!(
        fixture.mint_as_third(&service, &request, 31),
        Err(BlobError::QuotaExceeded)
    ));
    release(&fixture, &service, &reference, 32);
    assert_ne!(
        fixture.mint_as_third(&service, &request, 31).unwrap(),
        reference
    );
}

#[test]
fn identical_raw_keys_are_independent_across_authenticated_subjects() {
    let fixture = Fixture::new();
    let service = fixture.service(BlobConfig::default()).unwrap();
    let request = fixture.mint_request(b"aabb", 2, 3);
    let (_, first) = fixture.mint(&service, &request, 31).unwrap();
    let other_request = MintReferenceRequest {
        recipient_principal_id: fixture.issuer.principal_id.as_bytes().to_vec(),
        ..request
    };
    let second = fixture.mint_as_third(&service, &other_request, 31).unwrap();
    assert_ne!(first, second);
    let reader_one = fixture.read(&service, &first, 0, 2, 32).unwrap();
    let actor = fixture.mint_call(32);
    let reader_two = service
        .read_reference(&actor, &read_envelope(&actor, &second, 2, 2))
        .unwrap();
    assert_ne!(reader_one.receipt_id, reader_two.receipt_id);
    assert!(matches!(
        service.read_reference(&actor, &read_envelope(&actor, &first, 0, 2)),
        Err(BlobError::NotFound)
    ));
    assert_eq!(fixture.reads(), 2);
}

#[test]
fn release_during_mint_delivery_cannot_leave_a_published_pin() {
    use std::time::Duration;
    let fixture = Fixture::new();
    let service = fixture
        .service(BlobConfig {
            active_global: 1,
            ..BlobConfig::default()
        })
        .unwrap();
    let request = fixture.mint_request(b"aabb", 2, 3);
    let db_path = fixture.db_path();
    let once = AtomicBool::new(false);
    let (entered, reached) = std::sync::mpsc::sync_channel(1);
    let (resume, continue_mint) = std::sync::mpsc::sync_channel(1);
    let continue_mint = Mutex::new(continue_mint);
    *fixture.clock_hook.lock().unwrap() = Some(Arc::new(move || {
        if once.load(Ordering::SeqCst) {
            return;
        }
        let db = rusqlite::Connection::open(&db_path).unwrap();
        let count: i64 = db
            .query_row("SELECT count(*) FROM blob_tokens", [], |row| row.get(0))
            .unwrap();
        if count == 1 && !once.swap(true, Ordering::SeqCst) {
            entered.send(()).unwrap();
            continue_mint
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(3))
                .unwrap();
        }
    }));
    std::thread::scope(|scope| {
        let mint = scope.spawn(|| fixture.mint(&service, &request, 31));
        reached.recv_timeout(Duration::from_secs(3)).unwrap();
        let reference: Vec<u8> = fixture
            .db()
            .query_row("SELECT reference_id FROM blob_tokens", [], |row| row.get(0))
            .unwrap();
        let (started, release_started) = std::sync::mpsc::sync_channel(1);
        let (done, release_done) = std::sync::mpsc::sync_channel(1);
        let (call, wire, proof) = fixture.signed_release(fixture.issuer, &reference, 32);
        let service = &service;
        let releasing = scope.spawn(move || {
            started.send(()).unwrap();
            let outcome = service.release_reference(&call, &wire, Some(&proof));
            done.send(()).unwrap();
            outcome
        });
        release_started
            .recv_timeout(Duration::from_secs(3))
            .unwrap();
        // Both calls are active while mint is paused after commit. Release's
        // first clock sample must queue until the mint callback returns.
        let finished_while_clock_held =
            release_done.recv_timeout(Duration::from_millis(10)).is_ok();
        resume.send(()).unwrap();
        assert!(!finished_while_clock_held);
        releasing.join().unwrap().unwrap();
        assert!(matches!(
            fixture.read(service, &reference, 0, 2, 33),
            Err(BlobError::Released)
        ));
        fixture.mint(service, &request, 34).unwrap();
        assert_eq!(mint.join().unwrap().unwrap().1, reference);
    });
}

#[test]
fn clock_samples_advance_in_order_even_when_the_earlier_callback_is_paused() {
    use std::time::Duration;
    let fixture = Fixture::new();
    let (entered, reached) = std::sync::mpsc::sync_channel(1);
    let (resume, paused) = std::sync::mpsc::sync_channel(1);
    let paused = Mutex::new(paused);
    let calls = AtomicU64::new(0);
    let clock = Arc::new(move || {
        let wall_ms = 100 + calls.fetch_add(1, Ordering::SeqCst);
        let sample = ServiceTime {
            wall_ms,
            monotonic_ns: wall_ms * 1_000_000,
        };
        if wall_ms == 100 {
            entered.send(()).unwrap();
            paused
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(3))
                .unwrap();
        }
        Ok(sample)
    });
    let service = fixture
        .service_with_clock(Arc::clone(&fixture.authority), BlobConfig::default(), clock)
        .unwrap();

    std::thread::scope(|scope| {
        let older = scope.spawn(|| service.maintain());
        reached.recv_timeout(Duration::from_secs(3)).unwrap();
        let (done, completion) = std::sync::mpsc::sync_channel(1);
        let (started, waiter_started) = std::sync::mpsc::sync_channel(1);
        let service = &service;
        let newer = scope.spawn(move || {
            started.send(()).unwrap();
            let result = service.maintain();
            done.send(()).unwrap();
            result
        });
        waiter_started.recv_timeout(Duration::from_secs(3)).unwrap();
        let newer_first = completion.recv_timeout(Duration::from_millis(250)).is_ok();
        resume.send(()).unwrap();
        let earlier = older.join().unwrap();
        let later = newer.join().unwrap();
        eprintln!(
            "monotonic clock overlap: newer_first={newer_first}, earlier={earlier:?}, later={later:?}"
        );
        assert!(
            earlier.is_ok(),
            "an earlier legal sample must not look like rollback"
        );
        assert!(later.is_ok());
        assert!(!newer_first);
    });
}

#[test]
fn single_domain_clock_regressions_and_callback_errors_leave_both_watermarks_intact() {
    let fixture = Fixture::new();
    let sample = |wall_ms, monotonic_ns| {
        Ok(ServiceTime {
            wall_ms,
            monotonic_ns,
        })
    };
    let samples = Mutex::new(std::collections::VecDeque::from([
        sample(100, 100),
        sample(99, 500),
        sample(101, 101),
        sample(500, 100),
        sample(102, 102),
        Err(AuthorizationError::TimeUnavailable),
        sample(103, 103),
    ]));
    let clock = Arc::new(move || samples.lock().unwrap().pop_front().unwrap());
    let service = fixture
        .service_with_clock(Arc::clone(&fixture.authority), BlobConfig::default(), clock)
        .unwrap();
    assert_eq!(service.maintain().unwrap(), 0);
    for reason in ["wall rollback", "monotonic rollback", "clock error"] {
        let rejected = service.maintain();
        assert!(
            matches!(
                rejected,
                Err(BlobError::Authorization(
                    AuthorizationError::TimeUnavailable
                ))
            ),
            "{reason}: {rejected:?}"
        );
        assert_eq!(
            service.maintain().unwrap(),
            0,
            "{reason} altered a watermark"
        );
    }
}

fn pause_next_clock_sample(
    fixture: &Fixture,
) -> (
    std::sync::mpsc::Receiver<()>,
    std::sync::mpsc::SyncSender<()>,
) {
    use std::time::Duration;
    let (entered, reached) = std::sync::mpsc::sync_channel(1);
    let (resume, paused) = std::sync::mpsc::sync_channel(1);
    let paused = Mutex::new(paused);
    let once = AtomicBool::new(false);
    *fixture.clock_hook.lock().unwrap() = Some(Arc::new(move || {
        if !once.swap(true, Ordering::SeqCst) {
            entered.send(()).unwrap();
            paused
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(3))
                .unwrap();
        }
    }));
    (reached, resume)
}

#[test]
fn cold_clock_lock_contention_fails_with_a_bounded_time_unavailable() {
    use std::time::{Duration, Instant};
    let fixture = Fixture::new();
    let service = fixture.service(BlobConfig::default()).unwrap();
    let (reached, resume) = pause_next_clock_sample(&fixture);
    std::thread::scope(|scope| {
        let owner = scope.spawn(|| service.maintain());
        reached.recv_timeout(Duration::from_secs(3)).unwrap();
        let mut call = fixture.read_call(30);
        call.deadline_monotonic_ns = Some(120_000_000);
        let request = read_envelope(&call, &[7; 16], 0, 1);
        let started = Instant::now();
        let waiting = service.read_reference(&call, &request);
        let elapsed = started.elapsed();
        resume.send(()).unwrap();
        assert_eq!(owner.join().unwrap().unwrap(), 0);
        eprintln!("first clock sample lock contention: elapsed={elapsed:?}, waiting={waiting:?}");
        assert!(matches!(
            waiting,
            Err(BlobError::Authorization(
                AuthorizationError::TimeUnavailable
            ))
        ));
        assert!(elapsed < Duration::from_millis(250));
    });
}

#[test]
fn established_request_deadline_bounds_queued_clock_sampling() {
    use std::time::{Duration, Instant};
    let fixture = Fixture::new();
    let service = fixture.service(BlobConfig::default()).unwrap();
    let request = fixture.mint_request(b"a", 1, 2);
    let (_, reference) = fixture.mint(&service, &request, 31).unwrap();
    let (policy_entered, policy_resume) = pause_read_policy(&fixture, 0);

    std::thread::scope(|scope| {
        let reader = scope.spawn(|| {
            let mut call = fixture.read_call(32);
            call.deadline_monotonic_ns = Some(180_000_000);
            let request = read_envelope(&call, &reference, 0, 1);
            let started = Instant::now();
            let result = service.read_reference(&call, &request);
            (started.elapsed(), result)
        });
        policy_entered.recv_timeout(Duration::from_secs(3)).unwrap();
        let (clock_entered, clock_resume) = pause_next_clock_sample(&fixture);
        let clock_owner = scope.spawn(|| service.maintain());
        clock_entered.recv_timeout(Duration::from_secs(3)).unwrap();
        policy_resume.send(()).unwrap();
        let (elapsed, result) = reader.join().unwrap();
        clock_resume.send(()).unwrap();
        assert_eq!(clock_owner.join().unwrap().unwrap(), 0);
        eprintln!(
            "established clock lock contention: deadline=80ms elapsed={elapsed:?} result={result:?}"
        );
        assert!(matches!(
            result,
            Err(BlobError::Authorization(
                AuthorizationError::DeadlineExpired
            ))
        ));
        assert!(elapsed < Duration::from_millis(180));
    });
    assert_eq!(fixture.reads(), 0);
}
