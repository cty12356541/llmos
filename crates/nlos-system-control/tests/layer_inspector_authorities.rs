//! W32-G (B5-3) authority wiring: each per-layer inspection adapter is
//! driven against the real backing authority — the plan authority store,
//! the live tokio runtime, the durable `nlos-topic` rows, and the `nlos-store`
//! operation rows — and through the shared `SystemControl` handler path.

#![cfg(any(
    feature = "plan",
    feature = "runtime",
    feature = "topic",
    feature = "store"
))]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_schema::sabi::v1::{RetryDirective, SabiErrorCode, SabiFailure, SabiRequestContext};
use nlos_system_control::control::ControlCommand;
use nlos_system_control::control::dispatch_in_process;
use nlos_system_control::{RecoveryHealthSource, RecoverySystemControl, SystemControlAuthorizer};
use nlos_task::SqliteTaskAuthority;

static NEXT_DATABASE: AtomicU64 = AtomicU64::new(0);

struct TestFile {
    path: PathBuf,
}

impl TestFile {
    fn new(label: &str) -> Self {
        let sequence = NEXT_DATABASE.fetch_add(1, Ordering::Relaxed);
        Self {
            path: std::env::temp_dir().join(format!(
                "nlos-layer-inspectors-{label}-{}-{sequence}-{}.sqlite3",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|value| value.as_nanos())
                    .unwrap_or_default()
            )),
        }
    }
}

impl Drop for TestFile {
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

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new(label: &str) -> Self {
        let sequence = NEXT_DATABASE.fetch_add(1, Ordering::Relaxed);
        Self {
            path: std::env::temp_dir().join(format!(
                "nlos-layer-inspectors-{label}-{}-{sequence}",
                std::process::id()
            )),
        }
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn suffix_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

struct CapabilityPolicy;

fn authorize(context: &SabiRequestContext) -> Result<(), &'static str> {
    let expected = nlos_schema::sabi::v1::CapabilityHandle {
        slot: nlos_system_control::control::CONTROL_CAPABILITY_SLOT,
        generation: nlos_system_control::control::CONTROL_CAPABILITY_GENERATION,
    };
    if context.capability_handles.as_slice() == [expected] {
        Ok(())
    } else {
        Err("missing recovery operations capability")
    }
}

impl SystemControlAuthorizer for CapabilityPolicy {
    fn authorize_get(
        &self,
        context: &SabiRequestContext,
        _: &nlos_schema::sabi::v1::GetSystemControlRequest,
    ) -> Result<(), &'static str> {
        authorize(context)
    }

    fn authorize_submit(
        &self,
        context: &SabiRequestContext,
        _: &nlos_schema::sabi::v1::ControlCommand,
    ) -> Result<(), &'static str> {
        authorize(context)
    }
}

#[derive(Clone)]
struct StubHealth(nlos_commit_coordinator::RecoveryWorkerHealth);

impl RecoveryHealthSource for StubHealth {
    fn recovery_health(&self) -> nlos_commit_coordinator::RecoveryWorkerHealth {
        self.0.clone()
    }
}

fn stub_health() -> StubHealth {
    StubHealth(nlos_commit_coordinator::RecoveryWorkerHealth {
        state: nlos_commit_coordinator::RecoveryWorkerState::Running,
        ..Default::default()
    })
}

fn task_authority(label: &str) -> SqliteTaskAuthority {
    let file = TestFile::new(label);
    let authority = SqliteTaskAuthority::open(&file.path).expect("open task authority");
    std::mem::forget(file);
    authority
}

fn not_found(failure: &SabiFailure) -> bool {
    failure.code == i32::from(SabiErrorCode::NotFound)
        && failure.retry == i32::from(RetryDirective::DoNotRetry)
}

#[cfg(feature = "plan")]
mod plan_authority {
    use super::*;
    use nlos_plan::{
        ApplyPlanRevisionRequest, PlanNodeDeclaration, PlanNodeKind, PlanNodeState,
        PlanRevisionDecision, SqlitePlanAuthority,
    };
    use nlos_schema::sabi::v1::{
        ContextResidencyTier, PlanNodeKind as WireKind, PlanNodeLifecycleState as WireState,
    };
    use nlos_system_control::TaskNodeInspectSource as _;
    use nlos_system_control::control::{ControlOutcome, TaskNodeInspection};
    use nlos_system_control::plan_inspector::PlanAuthorityTaskNodeSource;
    use nlos_types::{IdempotencyKey, TaskNodeId, TaskPlanId};

    fn declared_plan() -> (SqlitePlanAuthority, TaskPlanId, TaskNodeId) {
        let file = TestFile::new("plan");
        let authority = SqlitePlanAuthority::open(&file.path).expect("open plan authority");
        std::mem::forget(file);
        let decision = authority
            .apply_plan_revision(ApplyPlanRevisionRequest {
                plan_id: None,
                nodes: vec![PlanNodeDeclaration {
                    node_key: [0x42; 16],
                    kind: PlanNodeKind::Executable,
                    binding_digest: [0x43; 32],
                    dependency_keys: Vec::new(),
                    input_selectors_digest: [0x44; 32],
                    output_contract_digest: [0x45; 32],
                    policy_digest: [0x49; 32],
                    resource_ceiling_digest: [0x4A; 32],
                    conditions: None,
                }],
                idempotency_key: IdempotencyKey::from_bytes([0x46; 16]),
                applied_at_ms: 1_000,
            })
            .expect("apply revision");
        let PlanRevisionDecision::Applied(receipt) = decision else {
            panic!("expected first revision to apply");
        };
        let nodes = authority
            .list_plan_nodes(receipt.plan_id)
            .expect("list nodes");
        assert_eq!(nodes.len(), 1);
        (authority, receipt.plan_id, nodes[0].node_id)
    }

    #[test]
    fn plan_source_reads_the_durable_node_row() {
        let (authority, plan_id, node_id) = declared_plan();
        let source = PlanAuthorityTaskNodeSource::new(&authority);
        let inspection = source
            .inspect_task_node(*plan_id.as_bytes(), *node_id.as_bytes())
            .expect("inspect node");
        assert_eq!(inspection.plan_id, *plan_id.as_bytes());
        assert_eq!(inspection.node_id, *node_id.as_bytes());
        assert_eq!(inspection.kind, WireKind::Executable);
        assert_eq!(inspection.state, WireState::Declared);
        assert_eq!(inspection.declared_revision, 1);
        assert_eq!(inspection.node_digest.len(), 32);
        assert_eq!(
            inspection.residency_tier,
            ContextResidencyTier::MetadataOnly
        );

        assert_eq!(
            PlanNodeState::Declared,
            authority
                .inspect_node(plan_id, node_id)
                .expect("inspect")
                .expect("present")
                .state
        );

        let missing = source
            .inspect_task_node(*plan_id.as_bytes(), [0xEE; 16])
            .expect_err("absent node must fail closed");
        assert!(not_found(&missing));
        let absent_plan = source
            .inspect_task_node([0xEE; 16], *node_id.as_bytes())
            .expect_err("absent plan must fail closed");
        assert!(not_found(&absent_plan));
        assert!(
            authority
                .inspect_node(
                    TaskPlanId::from_bytes([0xEE; 16]),
                    TaskNodeId::from_bytes([0xEE; 16])
                )
                .expect("inspect absent")
                .is_none()
        );
    }

    #[test]
    fn plan_source_serves_the_shared_handler_path() {
        let (authority, plan_id, node_id) = declared_plan();
        let tasks = task_authority("plan-handler");
        let health = stub_health();
        let source = PlanAuthorityTaskNodeSource::new(&authority);
        let control = RecoverySystemControl::new(&tasks, &health, &CapabilityPolicy)
            .with_task_node_source(&source);
        let receipt = dispatch_in_process(
            &control,
            &ControlCommand::InspectTaskNode {
                plan_id: *plan_id.as_bytes(),
                node_id: *node_id.as_bytes(),
            },
            10,
            6_000,
            None,
            None,
        )
        .expect("dispatch");
        let ControlOutcome::TaskNodeInspected(inspection): &ControlOutcome =
            receipt.outcome.as_ref().expect("success")
        else {
            panic!("expected task node inspection receipt");
        };
        let direct: TaskNodeInspection = source
            .inspect_task_node(*plan_id.as_bytes(), *node_id.as_bytes())
            .expect("direct read");
        assert_eq!(inspection, &direct);
    }
}

#[cfg(feature = "runtime")]
mod runtime_fiber {
    use super::*;
    use nlos_runtime::FiberSpec;
    use nlos_runtime::RuntimeAdapter as _;
    use nlos_runtime_tokio::{TokioRuntimeAdapter, TokioRuntimeConfig};
    use nlos_schema::sabi::v1::{ExecutionFiberLifecycleState, ExecutionFiberPhase};
    use nlos_system_control::ExecutionFiberInspectSource as _;
    use nlos_system_control::control::{ControlOutcome, ExecutionFiberInspection};
    use nlos_system_control::fiber_inspector::TokioExecutionFiberSource;
    use nlos_types::{
        AgentInstanceId, CancellationScopeId, ExecutionFiberId, Generation, ProcessId,
        ResourceGroupId, SchedulerDomainId,
    };

    fn id_bytes(value: u8) -> [u8; 16] {
        [value; 16]
    }

    fn fiber_spec(scope: CancellationScopeId) -> FiberSpec {
        FiberSpec {
            fiber_id: ExecutionFiberId::from_bytes(id_bytes(0xB1)),
            fiber_generation: Generation::INITIAL,
            agent_instance_id: AgentInstanceId::from_bytes(id_bytes(0xB2)),
            agent_generation: Generation::INITIAL,
            process_id: ProcessId::from_bytes(id_bytes(0xB3)),
            process_generation: Generation::INITIAL,
            task_attempt_id: None,
            cancellation_scope_id: scope,
            cancellation_generation: Generation::INITIAL,
            resource_group_id: ResourceGroupId::from_bytes(id_bytes(0xB4)),
            scheduler_domain_id: SchedulerDomainId::from_bytes(id_bytes(0xB5)),
            deadline: None,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fiber_source_reads_the_live_runtime_snapshot() {
        let runtime = TokioRuntimeAdapter::new(
            tokio::runtime::Handle::current(),
            TokioRuntimeConfig {
                max_live_fibers: 4,
                ..TokioRuntimeConfig::default()
            },
        )
        .expect("runtime");
        let scope = CancellationScopeId::from_bytes(id_bytes(0xB6));
        let spec = fiber_spec(scope);
        let (sender, receiver) = tokio::sync::oneshot::channel::<()>();
        let handle = runtime
            .spawn_fiber(spec, {
                Box::pin(async move {
                    let _ = receiver.await;
                    nlos_runtime::FiberExit::Completed
                })
            })
            .expect("spawn");

        let source = TokioExecutionFiberSource::new(&runtime);
        // The spawned future must first be polled at least once before its
        // state leaves `Created`/`Ready`; poll the bounded inspect surface
        // instead of asserting a racy snapshot. (`WaitingModel` is entered
        // only through the runtime's own channel/model-wait machinery, so a
        // plain parked future observably stays `Running`.)
        let mut inspection = None;
        for _ in 0..200 {
            let candidate = source
                .inspect_execution_fiber(*handle.fiber_id.as_bytes(), handle.generation.get())
                .expect("inspect fiber");
            if candidate.state == ExecutionFiberLifecycleState::Running {
                inspection = Some(candidate);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let inspection = inspection.expect("fiber was polled into the running state");
        assert_eq!(inspection.fiber_id, *handle.fiber_id.as_bytes());
        assert_eq!(inspection.generation, handle.generation.get());
        assert_eq!(inspection.state, ExecutionFiberLifecycleState::Running);
        assert_eq!(inspection.lifecycle_phase, ExecutionFiberPhase::Running);

        let unknown = source
            .inspect_execution_fiber([0xEE; 16], 1)
            .expect_err("unknown fiber must fail closed");
        assert!(not_found(&unknown));

        let zero = source
            .inspect_execution_fiber(*handle.fiber_id.as_bytes(), 0)
            .expect_err("zero generation must fail closed");
        assert_eq!(zero.code, i32::from(SabiErrorCode::InvalidArgument));

        let tasks = task_authority("fiber-handler");
        let health = stub_health();
        let control = RecoverySystemControl::new(&tasks, &health, &CapabilityPolicy)
            .with_execution_fiber_source(&source);
        let receipt = dispatch_in_process(
            &control,
            &ControlCommand::InspectExecutionFiber {
                fiber_id: *handle.fiber_id.as_bytes(),
                generation: handle.generation.get(),
            },
            10,
            6_000,
            None,
            None,
        )
        .expect("dispatch");
        let ControlOutcome::ExecutionFiberInspected(handler_inspection) =
            receipt.outcome.expect("success")
        else {
            panic!("expected execution fiber inspection receipt");
        };
        let direct: ExecutionFiberInspection = source
            .inspect_execution_fiber(*handle.fiber_id.as_bytes(), handle.generation.get())
            .expect("direct read");
        assert_eq!(handler_inspection, direct);

        let _ = sender.send(());
    }
}

#[cfg(feature = "topic")]
mod topic_rows {
    use super::*;
    use nlos_channel::{ChannelAuthority, CreateChannelRequest};
    use nlos_schema::sabi::v1::SabiFailure;
    use nlos_system_control::TopicInspectSource as _;
    use nlos_system_control::control::{ControlOutcome, TopicInspection};
    use nlos_system_control::topic_inspector::TopicAuthoritySource;
    use nlos_topic::{CreateTopicRequest, TopicAuthority, TopicId, TopicPolicy};
    use nlos_types::{ChannelId, IdempotencyKey, ResourceAccountId};

    fn created_topic() -> (
        TopicAuthority,
        std::sync::Arc<ChannelAuthority>,
        ChannelId,
        TopicId,
    ) {
        let root = TestDirectory::new("topic");
        std::fs::create_dir_all(&root.path).expect("create root");
        let channel = std::sync::Arc::new(ChannelAuthority::open(&root.path).expect("channel"));
        let topics =
            TopicAuthority::open(&root.path, std::sync::Arc::clone(&channel)).expect("topics");
        std::mem::forget(root);
        let channel_record = channel
            .create_channel(CreateChannelRequest {
                capacity_bytes: 1_024,
                policy_digest: [0x44; 32],
                idempotency_key: IdempotencyKey::from_bytes([0x45; 16]),
                created_at_ms: 1_000,
            })
            .expect("create channel")
            .record();
        let topic = topics
            .create_topic(CreateTopicRequest {
                channel_id: channel_record.channel_id,
                name: b"stage-b/w32g".to_vec(),
                policy: TopicPolicy {
                    max_recipients: 4,
                    delivery_attempts: 3,
                    cascade_depth: 2,
                    retained_bytes: 4_096,
                    retention_ms: 86_400_000,
                    payer: ResourceAccountId::from_bytes([0x47; 16]),
                },
                idempotency_key: IdempotencyKey::from_bytes([0x48; 16]),
                created_at_ms: 2_000,
            })
            .expect("create topic")
            .record();
        (topics, channel, channel_record.channel_id, topic.topic_id)
    }

    #[test]
    fn topic_source_reads_the_durable_row() {
        let (topics, _channel, channel_id, topic_id) = created_topic();
        let source = TopicAuthoritySource::new(&topics);
        let inspection = source
            .inspect_topic(*topic_id.as_bytes())
            .expect("inspect topic");
        assert_eq!(inspection.topic_id, *topic_id.as_bytes());
        assert_eq!(inspection.channel_id, *channel_id.as_bytes());
        assert_eq!(inspection.name, b"stage-b/w32g".to_vec());
        assert_eq!(inspection.active_subscriptions, 0);
        assert_eq!(inspection.policy_digest.len(), 32);

        let missing = source
            .inspect_topic([0xEE; 16])
            .expect_err("absent topic must fail closed");
        assert!(not_found(&missing));
        let _: SabiFailure = missing;
    }

    #[test]
    fn topic_source_serves_the_shared_handler_path() {
        let (topics, _channel, _channel_id, topic_id) = created_topic();
        let tasks = task_authority("topic-handler");
        let health = stub_health();
        let source = TopicAuthoritySource::new(&topics);
        let control = RecoverySystemControl::new(&tasks, &health, &CapabilityPolicy)
            .with_topic_source(&source);
        let receipt = dispatch_in_process(
            &control,
            &ControlCommand::InspectTopic {
                topic_id: *topic_id.as_bytes(),
            },
            10,
            6_000,
            None,
            None,
        )
        .expect("dispatch");
        let ControlOutcome::TopicInspected(inspection) = receipt.outcome.expect("success") else {
            panic!("expected topic inspection receipt");
        };
        let direct: TopicInspection = source
            .inspect_topic(*topic_id.as_bytes())
            .expect("direct read");
        assert_eq!(inspection, direct);
    }
}

#[cfg(feature = "store")]
mod operation_rows {
    use super::*;
    use nlos_operation::{OperationHandle, OperationSpec, OperationState};
    use nlos_schema::sabi::v1::DurableOperationState;
    use nlos_store::{RegistrationDecision, SqliteOperationStore};
    use nlos_system_control::OperationInspectSource as _;
    use nlos_system_control::control::{ControlOutcome, DurableOperationInspection};
    use nlos_system_control::operation_inspector::OperationStoreSource;
    use nlos_types::{CancellationScopeId, ExecutionFiberId, Generation, OperationId};

    fn registered_operation() -> (SqliteOperationStore, OperationHandle) {
        let file = TestFile::new("operation");
        let store = SqliteOperationStore::open(&file.path).expect("open operation store");
        std::mem::forget(file);
        let handle = OperationHandle {
            operation_id: OperationId::from_bytes([0xD1; 16]),
            generation: Generation::INITIAL,
        };
        let decision = store
            .register(OperationSpec {
                operation_id: handle.operation_id,
                generation: handle.generation,
                owner_fiber: nlos_runtime::FiberHandle {
                    fiber_id: ExecutionFiberId::from_bytes([0xB1; 16]),
                    generation: Generation::INITIAL,
                },
                cancellation_scope_id: CancellationScopeId::from_bytes([0xD2; 16]),
                cancellation_generation: Generation::INITIAL,
            })
            .expect("register");
        assert!(matches!(
            decision,
            RegistrationDecision::Created(_) | RegistrationDecision::Existing(_)
        ));
        (store, handle)
    }

    #[test]
    fn operation_source_reads_the_durable_state_machine_row() {
        let (store, handle) = registered_operation();
        let source = OperationStoreSource::new(&store);
        let inspection = source
            .inspect_operation(*handle.operation_id.as_bytes(), handle.generation.get())
            .expect("inspect operation");
        assert_eq!(inspection.operation_id, *handle.operation_id.as_bytes());
        assert_eq!(inspection.state, DurableOperationState::Registered);
        assert_eq!(inspection.owner_fiber_id, [0xB1; 16]);
        assert!(inspection.outcome_receipt_id.is_none());

        assert_eq!(
            store.inspect(handle).expect("direct inspect").state,
            OperationState::Registered
        );

        let missing = source
            .inspect_operation([0xEE; 16], 1)
            .expect_err("absent row must fail closed");
        assert!(not_found(&missing));

        let stale = source
            .inspect_operation(*handle.operation_id.as_bytes(), 9)
            .expect_err("stale generation must fail closed");
        assert!(not_found(&stale));
    }

    #[test]
    fn operation_source_serves_the_shared_handler_path() {
        let (store, handle) = registered_operation();
        let tasks = task_authority("operation-handler");
        let health = stub_health();
        let source = OperationStoreSource::new(&store);
        let control = RecoverySystemControl::new(&tasks, &health, &CapabilityPolicy)
            .with_operation_source(&source);
        let receipt = dispatch_in_process(
            &control,
            &ControlCommand::InspectOperation {
                operation_id: *handle.operation_id.as_bytes(),
                generation: handle.generation.get(),
            },
            10,
            6_000,
            None,
            None,
        )
        .expect("dispatch");
        let ControlOutcome::DurableOperationInspected(inspection) =
            receipt.outcome.expect("success")
        else {
            panic!("expected operation inspection receipt");
        };
        let direct: DurableOperationInspection = source
            .inspect_operation(*handle.operation_id.as_bytes(), handle.generation.get())
            .expect("direct read");
        assert_eq!(inspection, direct);
    }
}
