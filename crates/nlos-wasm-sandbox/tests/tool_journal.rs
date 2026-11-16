use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_capability::service_call::RuntimeCaller;
use nlos_schema::sabi::v1::InvokeToolResult;
use nlos_schema::{encode_invoke_tool_result, tool_sandbox_schema_identity};
use nlos_types::{
    ApplicationId, Generation, IdempotencyKey, NamespaceId, PrincipalId, ProcessId, SessionId,
};
use nlos_wasm_sandbox::tool_operation_store::{
    BeginDecision, DirectoryEvidence, DurableToolResult, JournalToolOperationStore, StoreError,
    ToolFailure, ToolJournalLimits, ToolOperation, ToolOperationKey, ToolOperationSpec,
    ToolOperationState, ToolOperationStore, ToolOwner,
};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "nlos-tool-journal-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir).unwrap();
        Self(dir)
    }
    fn path(&self) -> PathBuf {
        self.0.join("ops")
    }
    fn open(&self) -> JournalToolOperationStore {
        JournalToolOperationStore::open(self.path(), ToolJournalLimits::default()).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn identity(marker: u8, raw_key: u8) -> (ToolOperationKey, ToolOperationSpec) {
    let caller = RuntimeCaller {
        principal: PrincipalId::from_bytes([marker; 16]),
        application: ApplicationId::from_bytes([2; 16]),
        process: ProcessId::from_bytes([3; 16]),
        process_generation: Generation::INITIAL,
        session: SessionId::from_bytes([4; 16]),
    };
    let key = ToolOperationKey {
        owner: ToolOwner {
            principal: caller.principal,
            application: caller.application,
            namespace: NamespaceId::from_bytes([5; 16]),
        },
        registration: [6; 16],
        key: IdempotencyKey::from_bytes([raw_key; 16]),
    };
    (
        key,
        ToolOperationSpec {
            origin: caller,
            wasm_digest: [7; 32],
            registry_generation: Generation::INITIAL,
        },
    )
}

fn begin(store: &JournalToolOperationStore, marker: u8, raw_key: u8) -> ToolOperation {
    let (key, spec) = identity(marker, raw_key);
    let BeginDecision::Registered(op) = store.begin(key, [8; 32], spec).unwrap() else {
        panic!("unexpected replay")
    };
    op
}

fn result(output_json: String) -> ToolOperationState {
    let wire = encode_invoke_tool_result(&InvokeToolResult {
        schema: Some(tool_sandbox_schema_identity()),
        output_json,
    })
    .unwrap();
    ToolOperationState::Completed(DurableToolResult::from_wire(wire).unwrap())
}

#[test]
fn reopen_reconciles_each_unfinished_state_and_preserves_original_completed_result() {
    let fixture = Fixture::new();
    let store = fixture.open();
    let registered = begin(&store, 1, 1);
    let dispatched = store
        .mark_dispatched(&begin(&store, 1, 2), Some([19; 32]))
        .unwrap();
    let cancel = store
        .mark_dispatched(&begin(&store, 1, 3), Some([20; 32]))
        .unwrap();
    let cancel = store.request_cancel(&cancel).unwrap();
    let completed = store.mark_dispatched(&begin(&store, 1, 4), None).unwrap();
    let completed = store
        .complete(&completed, result(r#"{"v":42}"#.into()))
        .unwrap();

    assert!(matches!(
        JournalToolOperationStore::open(fixture.path(), ToolJournalLimits::default()),
        Err(StoreError::Busy)
    ));
    drop(store);

    let store = fixture.open();
    assert_eq!(
        store.query(registered.handle).unwrap().state,
        ToolOperationState::Failed(ToolFailure::Interrupted)
    );
    assert_eq!(
        store.query(dispatched.handle).unwrap().state,
        ToolOperationState::EffectUnknown {
            reason: ToolFailure::HostLost,
            directory: DirectoryEvidence {
                before: Some([19; 32]),
                after: None
            }
        }
    );
    assert_eq!(
        store.query(cancel.handle).unwrap().state,
        ToolOperationState::EffectUnknown {
            reason: ToolFailure::Cancelled,
            directory: DirectoryEvidence {
                before: Some([20; 32]),
                after: None
            }
        }
    );
    assert_eq!(store.query(completed.handle).unwrap(), completed);
    assert_eq!(
        store
            .begin(completed.key, completed.request_digest, completed.spec)
            .unwrap(),
        BeginDecision::Existing(completed.clone())
    );
    let unchanged =
        [registered, dispatched, cancel].map(|operation| store.query(operation.handle).unwrap());
    drop(store);

    let store = fixture.open();
    for recovered in unchanged {
        assert_eq!(store.query(recovered.handle).unwrap(), recovered);
        assert_eq!(
            store.begin(recovered.key, [99; 32], recovered.spec),
            Err(StoreError::Conflict)
        );
        assert_eq!(
            store.mark_dispatched(&recovered, None),
            Err(StoreError::InvalidTransition)
        );
    }
}

#[test]
fn concurrent_begin_returns_one_dispatchable_receipt_and_cas_rejects_late_completion() {
    let fixture = Fixture::new();
    let store = fixture.open();
    let (key, spec) = identity(1, 1);
    let race = std::sync::Barrier::new(2);
    let decisions = std::thread::scope(|scope| {
        let first = scope.spawn(|| {
            race.wait();
            store.begin(key, [8; 32], spec).unwrap()
        });
        race.wait();
        let second = store.begin(key, [8; 32], spec).unwrap();
        [first.join().unwrap(), second]
    });
    let op = match decisions {
        [
            BeginDecision::Registered(op),
            BeginDecision::Existing(other),
        ]
        | [
            BeginDecision::Existing(other),
            BeginDecision::Registered(op),
        ] => {
            assert_eq!(op, other);
            op
        }
        _ => panic!("only one registrar should obtain the dispatchable operation"),
    };
    let dispatched = store.mark_dispatched(&op, None).unwrap();
    assert_eq!(store.mark_dispatched(&op, None), Err(StoreError::Stale));
    let cancel = store.request_cancel(&dispatched).unwrap();
    assert_eq!(
        store.complete(&dispatched, result("0".into())),
        Err(StoreError::Stale)
    );
    let unknown = store
        .complete(
            &cancel,
            ToolOperationState::EffectUnknown {
                reason: ToolFailure::Cancelled,
                directory: DirectoryEvidence::default(),
            },
        )
        .unwrap();
    assert_eq!(
        store.complete(&dispatched, result("0".into())).unwrap(),
        unknown
    );
    assert_eq!(store.request_cancel(&dispatched).unwrap(), unknown);
    assert_ne!(begin(&store, 99, 1).handle, op.handle);
}

#[test]
fn watermark_refuses_admission_but_reserves_a_large_complete_result() {
    for limits in [
        ToolJournalLimits {
            admission_bytes: 400,
            admission_records: 15,
            hard_bytes: 96 * 1024,
            hard_records: 16,
        },
        ToolJournalLimits {
            admission_bytes: 800,
            admission_records: 1,
            hard_bytes: 128 * 1024,
            hard_records: 4,
        },
    ] {
        let fixture = Fixture::new();
        let store = JournalToolOperationStore::open(fixture.path(), limits).unwrap();
        let first = begin(&store, 1, 1);
        let (key, spec) = identity(1, 2);
        assert_eq!(store.begin(key, [8; 32], spec), Err(StoreError::Quota));
        let dispatch = store.mark_dispatched(&first, None).unwrap();
        let done = store
            .complete(&dispatch, result(format!("\"{}\"", "a".repeat(65_000))))
            .unwrap();
        assert!(fs::metadata(fixture.path()).unwrap().len() > limits.admission_bytes);
        drop(store);
        let store = JournalToolOperationStore::open(fixture.path(), limits).unwrap();
        assert_eq!(store.query(done.handle).unwrap(), done);
        assert_eq!(store.begin(key, [8; 32], spec), Err(StoreError::Quota));
    }
}

#[test]
fn torn_tail_can_be_repaired_but_middle_corruption_and_bad_sequences_fail_closed() {
    let fixture = Fixture::new();
    let store = fixture.open();
    let op = store.mark_dispatched(&begin(&store, 1, 1), None).unwrap();
    let completed = store.complete(&op, result("42".into())).unwrap();
    drop(store);
    let path = fixture.path();
    let committed = fs::read(&path).unwrap();
    for torn in [b"N".as_slice(), b"NTO1"] {
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(torn)
            .unwrap();
        let reopened = fixture.open();
        assert_eq!(reopened.query(op.handle).unwrap(), completed);
        drop(reopened);
        assert_eq!(fs::read(&path).unwrap(), committed);
    }
    // Remove only the end of the completed record and recover its dispatch fence.
    fs::write(&path, &committed[..committed.len() - 4]).unwrap();
    let recovered = fixture.open();
    assert!(matches!(
        recovered.query(op.handle).unwrap().state,
        ToolOperationState::EffectUnknown {
            reason: ToolFailure::HostLost,
            ..
        }
    ));
    drop(recovered);

    for offset in [12, 60] {
        let mut broken = committed.clone();
        broken[offset] ^= 0x80;
        fs::write(&path, broken).unwrap();
        assert!(matches!(
            JournalToolOperationStore::open(&path, ToolJournalLimits::default()),
            Err(StoreError::Corrupt)
        ));
    }
}

#[test]
fn crash_fixture_child() {
    let Some(path) = std::env::var_os("NLOS_W82_JOURNAL_CRASH_PATH") else {
        return;
    };
    let stage = std::env::var("NLOS_W82_JOURNAL_CRASH_STAGE").unwrap();
    let path = PathBuf::from(path);
    let store = JournalToolOperationStore::open(&path, ToolJournalLimits::default()).unwrap();
    if stage != "empty" {
        let op = begin(&store, 1, 1);
        if stage != "registered" {
            let dispatched = store.mark_dispatched(&op, None).unwrap();
            match stage.as_str() {
                "cancel" => {
                    store.request_cancel(&dispatched).unwrap();
                }
                "completed" => {
                    store.complete(&dispatched, result("42".into())).unwrap();
                }
                _ => {}
            }
        }
    }
    fs::write(path.with_extension("ready"), []).unwrap();
    loop {
        std::thread::park();
    }
}

#[test]
fn subprocess_loss_recovers_at_each_durable_transaction_boundary() {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    for stage in ["empty", "registered", "dispatched", "cancel", "completed"] {
        let fixture = Fixture::new();
        let path = fixture.path();
        let mut process = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "crash_fixture_child"])
            .env("NLOS_W82_JOURNAL_CRASH_PATH", &path)
            .env("NLOS_W82_JOURNAL_CRASH_STAGE", stage)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let started = Instant::now();
        while !path.with_extension("ready").exists() {
            if started.elapsed() >= Duration::from_secs(5) {
                let _ = process.kill();
                let _ = process.wait();
                panic!("child did not reach {stage} transaction");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        process.kill().unwrap();
        process.wait().unwrap();

        let store = fixture.open();
        let (key, spec) = identity(1, 1);
        let observed = store
            .find(key, [8; 32], spec)
            .unwrap()
            .map(|operation| operation.state);
        let expected = match stage {
            "registered" => Some(ToolOperationState::Failed(ToolFailure::Interrupted)),
            "dispatched" => Some(ToolOperationState::EffectUnknown {
                reason: ToolFailure::HostLost,
                directory: DirectoryEvidence::default(),
            }),
            "cancel" => Some(ToolOperationState::EffectUnknown {
                reason: ToolFailure::Cancelled,
                directory: DirectoryEvidence::default(),
            }),
            "completed" => Some(result("42".into())),
            _ => None,
        };
        assert_eq!(observed, expected, "stage {stage}");
    }
}

#[test]
fn checksum_intact_duplicate_journal_frame_is_rejected() {
    let fixture = Fixture::new();
    let store = fixture.open();
    begin(&store, 1, 1);
    drop(store);

    let path = fixture.path();
    let frame = fs::read(&path).unwrap();
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(&frame)
        .unwrap();
    assert!(matches!(
        JournalToolOperationStore::open(path, ToolJournalLimits::default()),
        Err(StoreError::Corrupt)
    ));
}
