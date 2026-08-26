//! Fault-prefix and restart coverage for the Semantic publication coordinator.
//!
//! The baseline coordinator fixture is included in this integration binary so
//! the fault cases can reuse its authority-first setup without widening the
//! production write set.  The extra cases below intentionally exercise one
//! durable boundary at a time: Semantic owner publication, Task-side owner
//! receipt consumption, and terminal Task finalization.

#[allow(deprecated)] // ladder constructors deprecated in favor of the struct entries
mod matrix {
    include!("semantic_convergence.rs");

    use std::fs;
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Child, Command, Stdio};
    use std::time::Duration;

    use nlos_capability::CapabilityTarget;
    use nlos_semantic::{PublishSemanticPublicationRequest, SemanticAuthorityError};

    fn register_fault_vfs() {
        nlos_store_fault::register(VFS_NAME).expect("register semantic coordinator fault VFS");
    }

    fn semantic_database(root: &Path) -> PathBuf {
        root.join("semantic-authority.db")
    }

    fn sibling_path(path: &Path, suffix: &str) -> PathBuf {
        let mut value = path.as_os_str().to_os_string();
        value.push(suffix);
        PathBuf::from(value)
    }

    fn wal_commit_frames(wal: &[u8]) -> (usize, Vec<usize>) {
        assert!(wal.len() >= 32, "WAL must have a header");
        let page_size = match u32::from_be_bytes(wal[8..12].try_into().expect("page size field")) {
            1 => 65_536,
            value => value as usize,
        };
        assert!(page_size >= 512, "valid SQLite page size");
        let frame_size = 24 + page_size;
        let frame_count = (wal.len() - 32) / frame_size;
        assert!(frame_count > 0, "fixture must contain WAL frames");
        let commits: Vec<usize> = (0..frame_count)
            .filter(|index| {
                let start = 32 + index * frame_size;
                u32::from_be_bytes(wal[start + 8..start + 12].try_into().expect("commit field"))
                    != 0
            })
            .collect();
        assert!(commits.len() >= 2, "fixture must contain several commits");
        (frame_size, commits)
    }

    fn truncate_wal_inside_last_commit(root: &Path) {
        let database = semantic_database(root);
        let wal_path = sibling_path(&database, "-wal");
        let mut wal = fs::read(&wal_path).expect("read Semantic WAL");
        let (frame_size, commits) = wal_commit_frames(&wal);
        let last_commit = *commits.last().expect("WAL commit exists");
        let half_frame = 32 + last_commit * frame_size + frame_size / 2;
        wal.truncate(half_frame);
        fs::write(&wal_path, wal).expect("truncate Semantic WAL tail");
        fs::remove_file(sibling_path(&database, "-shm")).expect("remove stale Semantic SHM");
    }

    fn publication_ids(root: &Path) -> Vec<Vec<u8>> {
        let database = semantic_database(root);
        let connection = Connection::open(database).expect("open Semantic inspection connection");
        let mut statement = connection
            .prepare(
                "SELECT receipt_id FROM semantic_publication_receipts
                 ORDER BY receipt_id",
            )
            .expect("prepare publication inspection");
        statement
            .query_map([], |row| row.get::<_, Vec<u8>>(0))
            .expect("query publication receipts")
            .collect::<Result<Vec<_>, _>>()
            .expect("decode publication receipts")
    }

    fn task_receipt_ids(path: &Path) -> Vec<Vec<u8>> {
        let connection = Connection::open(path).expect("open Task inspection connection");
        let mut statement = connection
            .prepare("SELECT receipt_id FROM task_receipts ORDER BY receipt_id")
            .expect("prepare Task receipt inspection");
        statement
            .query_map([], |row| row.get::<_, Vec<u8>>(0))
            .expect("query Task receipts")
            .collect::<Result<Vec<_>, _>>()
            .expect("decode Task receipts")
    }

    fn hex(bytes: &[u8]) -> String {
        use std::fmt::Write as _;

        bytes.iter().fold(String::new(), |mut output, byte| {
            write!(output, "{byte:02x}").expect("write plan id hex");
            output
        })
    }

    fn decode_plan_id(encoded: &str) -> SemanticCommitPlanId {
        assert_eq!(encoded.len(), 32, "plan id must be 16 bytes of hex");
        let mut bytes = [0_u8; 16];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&encoded[index * 2..index * 2 + 2], 16)
                .expect("valid plan id hex");
        }
        SemanticCommitPlanId::from_bytes(bytes)
    }

    fn spawn_torn_wal_child(fixture: &Fixture, plan_id: SemanticCommitPlanId) -> Child {
        Command::new(std::env::current_exe().expect("current test executable"))
            .args(["--exact", "matrix::semantic_torn_wal_child", "--nocapture"])
            .env("NLOS_SEMANTIC_TORN_TASK_PATH", &fixture.task_path)
            .env("NLOS_SEMANTIC_TORN_ROOT", &fixture.semantic_root)
            .env("NLOS_SEMANTIC_TORN_PLAN", hex(plan_id.as_bytes()))
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn Semantic torn-WAL child")
    }

    fn await_ready(child: &mut Child) {
        let stdout = child.stdout.take().expect("piped child stdout");
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let marker = BufReader::new(stdout)
                .lines()
                .find_map(|line| match line {
                    Ok(line) if line.starts_with("READY") => Some(Ok(())),
                    Ok(_) => None,
                    Err(error) => Some(Err(error.to_string())),
                })
                .unwrap_or_else(|| Err("child exited without READY".to_string()));
            let _ = sender.send(marker);
        });
        match receiver.recv_timeout(Duration::from_mins(1)) {
            Ok(Ok(())) => {}
            other => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("Semantic torn-WAL child did not report READY: {other:?}");
            }
        }
    }

    fn kill_and_reap(child: &mut Child) {
        child
            .kill()
            .expect("force-terminate Semantic torn-WAL child");
        let status = child.wait().expect("wait for Semantic torn-WAL child");
        assert!(
            !status.success(),
            "killed Semantic torn-WAL child exited successfully"
        );
    }

    fn authorize(
        task: &SqliteTaskAuthority,
        semantic: &SemanticAuthority,
        plan_id: SemanticCommitPlanId,
    ) {
        let step = SemanticCommitCoordinator::new(task, semantic)
            .converge_one_step(ConvergeSemanticCommitRequest { plan_id, now_ms: 7 })
            .expect("authorize Semantic publication plan");
        assert!(matches!(step, ConvergeSemanticStep::Authorized));
    }

    fn publish_request(
        task: &SqliteTaskAuthority,
        plan_id: SemanticCommitPlanId,
        published_at_ms: u64,
    ) -> PublishSemanticPublicationRequest {
        let progress = task
            .inspect_semantic_commit_progress(plan_id)
            .expect("inspect Semantic plan");
        let expectation = task
            .inspect_semantic_commit_expectations(plan_id)
            .expect("inspect Semantic expectations")
            .into_iter()
            .next()
            .expect("one Semantic expectation");
        PublishSemanticPublicationRequest {
            task_id: progress.plan.task_id,
            permit_id: progress.plan.permit_id,
            write_set_root: progress.plan.write_set_root,
            event_id: expectation.event_id,
            target: match expectation.target {
                TaskWriteSetSemanticTarget::Namespace(namespace) => {
                    CapabilityTarget::Namespace(namespace)
                }
                TaskWriteSetSemanticTarget::Task(task) => CapabilityTarget::Task(task),
            },
            admission_receipt_id: expectation.admission_receipt_id,
            durability_receipt_id: expectation.durability_receipt_id,
            published_at_ms,
        }
    }

    fn assert_finalized(
        task: &SqliteTaskAuthority,
        plan_id: SemanticCommitPlanId,
        expected_publication_count: usize,
    ) {
        let progress = task
            .inspect_semantic_commit_progress(plan_id)
            .expect("inspect finalized Semantic plan");
        assert_eq!(progress.plan.state, SemanticCommitPlanState::Finalized);
        assert_eq!(progress.publications.len(), expected_publication_count);
        assert_eq!(
            task.inspect_task(progress.plan.task_id)
                .unwrap()
                .head_commit_seq,
            1
        );
        assert!(progress.plan.task_receipt_id.is_some());
    }

    #[test]
    fn semantic_owner_ioerr_and_enospc_leave_no_phantom_and_retry() {
        let _serialization = fault_lock();
        register_fault_vfs();
        for code in [FaultCode::IoErr, FaultCode::Full] {
            nlos_store_fault::disarm();
            let _fault_guard = FaultDisarmGuard;
            let fixture = Fixture::new();
            let (task, semantic, plan_id, ..) = prepare(&fixture, false);
            authorize(&task, &semantic, plan_id);
            drop(semantic);

            let faulted = SemanticAuthority::open_with_vfs(&fixture.semantic_root, Some(VFS_NAME))
                .expect("reopen Semantic authority through fault VFS");
            nlos_store_fault::arm(FaultMode::FailWritesAfter { remaining: 0, code });
            let result = SemanticCommitCoordinator::new(&task, &faulted)
                .converge_one_step(ConvergeSemanticCommitRequest { plan_id, now_ms: 8 });
            assert!(matches!(
                result,
                Err(nlos_commit_coordinator::CoordinatorError::Semantic(
                    SemanticAuthorityError::Sqlite(_)
                ))
            ));
            assert!(nlos_store_fault::writes_observed() > 0);
            drop(faulted);
            nlos_store_fault::disarm();

            let reopened = SemanticAuthority::open(&fixture.semantic_root)
                .expect("reopen Semantic authority after write failure");
            assert!(publication_ids(&fixture.semantic_root).is_empty());
            assert!(matches!(
                task.inspect_semantic_commit_progress(plan_id),
                Ok(progress) if progress.plan.state == SemanticCommitPlanState::Publishing
                    && progress.publications.is_empty()
            ));
            let receipt = SemanticCommitCoordinator::new(&task, &reopened)
                .converge(ConvergeSemanticCommitRequest { plan_id, now_ms: 9 })
                .expect("retry after Semantic owner failure");
            assert_eq!(receipt.semantic_publications.len(), 1);
            assert_finalized(&task, plan_id, 1);
        }
    }

    #[test]
    fn semantic_owner_power_loss_and_torn_wal_tail_are_not_published() {
        let _serialization = fault_lock();
        register_fault_vfs();

        // The VFS's silent-loss mode models a successful write response whose
        // bytes never reach durable storage. The coordinator must only consume
        // the owner receipt after a fresh authority incarnation can read it.
        nlos_store_fault::disarm();
        let _fault_guard = FaultDisarmGuard;
        let fixture = Fixture::new();
        let (task, semantic, plan_id, ..) = prepare(&fixture, false);
        authorize(&task, &semantic, plan_id);
        drop(semantic);
        let faulted = SemanticAuthority::open_with_vfs(&fixture.semantic_root, Some(VFS_NAME))
            .expect("reopen Semantic authority through fault VFS");
        nlos_store_fault::arm(FaultMode::PowerLossAfter { remaining: 0 });
        let request = publish_request(&task, plan_id, 8);
        let _ = faulted
            .publish_semantic_publication(request)
            .expect("silent-loss publication call returns its local decision");
        assert!(nlos_store_fault::writes_observed() > 0);
        drop(faulted);
        nlos_store_fault::disarm();

        let reopened = SemanticAuthority::open(&fixture.semantic_root)
            .expect("reopen Semantic authority after silent loss");
        assert!(publication_ids(&fixture.semantic_root).is_empty());
        let recovered = SemanticCommitCoordinator::new(&task, &reopened)
            .converge(ConvergeSemanticCommitRequest { plan_id, now_ms: 9 })
            .expect("retry after silent loss");
        assert_eq!(recovered.semantic_publications.len(), 1);
        assert_finalized(&task, plan_id, 1);
        let durable_ids = publication_ids(&fixture.semantic_root);
        assert_eq!(durable_ids.len(), 1);

        // A torn WAL tail is applied after a normal owner publication. The
        // last commit must disappear on reopen, after which the same plan
        // converges from PUBLISHING and creates exactly one receipt.
        let fixture = Fixture::new();
        let (task, semantic, plan_id, ..) = prepare(&fixture, false);
        authorize(&task, &semantic, plan_id);
        drop(semantic);
        drop(task);
        let mut child = spawn_torn_wal_child(&fixture, plan_id);
        await_ready(&mut child);
        kill_and_reap(&mut child);
        truncate_wal_inside_last_commit(&fixture.semantic_root);
        let reopened = SemanticAuthority::open(&fixture.semantic_root)
            .expect("reopen Semantic authority after torn WAL");
        let task = SqliteTaskAuthority::open(&fixture.task_path)
            .expect("reopen Task authority after torn WAL");
        assert!(publication_ids(&fixture.semantic_root).is_empty());
        let recovered = SemanticCommitCoordinator::new(&task, &reopened)
            .converge(ConvergeSemanticCommitRequest {
                plan_id,
                now_ms: 19,
            })
            .expect("retry after torn WAL");
        assert_eq!(recovered.semantic_publications.len(), 1);
        assert_finalized(&task, plan_id, 1);
        assert_eq!(publication_ids(&fixture.semantic_root).len(), 1);
    }

    #[test]
    fn semantic_torn_wal_child() {
        let (Ok(task_path), Ok(semantic_root), Ok(plan)) = (
            std::env::var("NLOS_SEMANTIC_TORN_TASK_PATH"),
            std::env::var("NLOS_SEMANTIC_TORN_ROOT"),
            std::env::var("NLOS_SEMANTIC_TORN_PLAN"),
        ) else {
            return;
        };
        let task = SqliteTaskAuthority::open(task_path).expect("open Semantic torn-WAL Task");
        let semantic = SemanticAuthority::open(semantic_root).expect("open Semantic torn-WAL");
        let plan_id = decode_plan_id(&plan);
        semantic
            .publish_semantic_publication(publish_request(&task, plan_id, 18))
            .expect("publish Semantic owner prefix in child");
        println!("READY semantic-owner-publication");
        std::io::stdout()
            .flush()
            .expect("flush Semantic child marker");
        let _keepers = (task, semantic);
        loop {
            std::thread::park();
        }
    }

    #[test]
    fn semantic_owner_commit_then_task_consumer_failure_replays_one_receipt() {
        let _serialization = fault_lock();
        register_fault_vfs();
        nlos_store_fault::disarm();
        let _fault_guard = FaultDisarmGuard;
        let fixture = Fixture::new();
        let (task, semantic, plan_id, ..) = prepare(&fixture, false);
        authorize(&task, &semantic, plan_id);
        drop(task);

        let faulted = SqliteTaskAuthority::open_with_vfs(&fixture.task_path, Some(VFS_NAME))
            .expect("reopen Task authority through fault VFS");
        nlos_store_fault::arm(FaultMode::FailWritesAfter {
            remaining: 0,
            code: FaultCode::IoErr,
        });
        let result = SemanticCommitCoordinator::new(&faulted, &semantic)
            .converge_one_step(ConvergeSemanticCommitRequest { plan_id, now_ms: 8 });
        assert!(matches!(
            result,
            Err(nlos_commit_coordinator::CoordinatorError::Task(_))
        ));
        assert!(nlos_store_fault::writes_observed() > 0);
        drop(faulted);
        nlos_store_fault::disarm();

        let reopened = SqliteTaskAuthority::open(&fixture.task_path)
            .expect("reopen Task authority after consumer failure");
        let progress = reopened
            .inspect_semantic_commit_progress(plan_id)
            .expect("inspect Task prefix");
        assert_eq!(progress.plan.state, SemanticCommitPlanState::Publishing);
        assert!(progress.publications.is_empty());
        assert_eq!(publication_ids(&fixture.semantic_root).len(), 1);
        let receipt = SemanticCommitCoordinator::new(&reopened, &semantic)
            .converge(ConvergeSemanticCommitRequest { plan_id, now_ms: 9 })
            .expect("replay owner publication and consume it once");
        assert_eq!(receipt.semantic_publications.len(), 1);
        assert_finalized(&reopened, plan_id, 1);
        assert_eq!(publication_ids(&fixture.semantic_root).len(), 1);
        let replay = SemanticCommitCoordinator::new(&reopened, &semantic)
            .converge(ConvergeSemanticCommitRequest {
                plan_id,
                now_ms: 10,
            })
            .expect("exactly replay finalized Task receipt");
        assert_eq!(replay, receipt);
        assert_eq!(task_receipt_ids(&fixture.task_path).len(), 1);
    }

    #[test]
    fn semantic_terminal_finalize_failure_keeps_ready_and_retries_once() {
        let _serialization = fault_lock();
        register_fault_vfs();
        nlos_store_fault::disarm();
        let _fault_guard = FaultDisarmGuard;
        let fixture = Fixture::new();
        let (task, semantic, plan_id, ..) = prepare(&fixture, false);
        let coordinator = SemanticCommitCoordinator::new(&task, &semantic);
        authorize(&task, &semantic, plan_id);
        assert!(matches!(
            coordinator
                .converge_one_step(ConvergeSemanticCommitRequest { plan_id, now_ms: 8 })
                .expect("publish owner receipt"),
            ConvergeSemanticStep::PublishedOne {
                state_after: SemanticCommitPlanState::Ready,
                ..
            }
        ));
        drop(task);

        let faulted = SqliteTaskAuthority::open_with_vfs(&fixture.task_path, Some(VFS_NAME))
            .expect("reopen Task authority through fault VFS");
        nlos_store_fault::arm(FaultMode::FailWritesAfter {
            remaining: 0,
            code: FaultCode::Full,
        });
        let result = SemanticCommitCoordinator::new(&faulted, &semantic)
            .converge_one_step(ConvergeSemanticCommitRequest { plan_id, now_ms: 9 });
        assert!(matches!(
            result,
            Err(nlos_commit_coordinator::CoordinatorError::Task(_))
        ));
        drop(faulted);
        nlos_store_fault::disarm();

        let reopened = SqliteTaskAuthority::open(&fixture.task_path)
            .expect("reopen Task authority after finalize failure");
        let progress = reopened
            .inspect_semantic_commit_progress(plan_id)
            .expect("inspect READY prefix");
        assert_eq!(progress.plan.state, SemanticCommitPlanState::Ready);
        assert_eq!(progress.publications.len(), 1);
        assert_eq!(
            reopened
                .inspect_task(progress.plan.task_id)
                .unwrap()
                .head_commit_seq,
            0
        );
        let receipt = SemanticCommitCoordinator::new(&reopened, &semantic)
            .converge(ConvergeSemanticCommitRequest {
                plan_id,
                now_ms: 10,
            })
            .expect("retry terminal finalize");
        assert_eq!(receipt.semantic_publications.len(), 1);
        assert_finalized(&reopened, plan_id, 1);
        assert_eq!(publication_ids(&fixture.semantic_root).len(), 1);
        let replay = SemanticCommitCoordinator::new(&reopened, &semantic)
            .converge(ConvergeSemanticCommitRequest {
                plan_id,
                now_ms: 11,
            })
            .expect("exactly replay finalized Task receipt");
        assert_eq!(replay, receipt);
        assert_eq!(task_receipt_ids(&fixture.task_path).len(), 1);
    }
}
