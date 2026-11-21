//! U-12 (`DEL-DATA`) cross-process concurrent orphan-GC vs revision write/stage
//! verification with real OS child processes.
//!
//! Each test spawns independent OS processes via `std::env::current_exe()`
//! running the `#[ignore]` child entry point `u12_cross_process_child_helper`
//! (`--exact u12_cross_process_child_helper --ignored --nocapture`). All waits
//! carry bounded timeouts and a RAII child guard reaps any remaining child on
//! failure or panic before `TestStoreDir` cleans up the temporary store root.

mod support;

use std::collections::BTreeSet;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use nlos_artifact::{
    ArtifactStore, AutoGcSkipReason, AutoGcTickDecision, AutoOrphanGcPolicy,
    CollectOrphanBlobsDecision, CollectOrphanBlobsRequest, ContentDigest,
    PublishStagedRevisionDecision, PublishStagedRevisionRequest, PutRevisionDecision,
    PutRevisionRequest, StageRevisionDecision, StageRevisionRequest, TickAutoGcRequest,
    staging_id_for,
};
use nlos_types::{ArtifactId, CommitPermitId, IdempotencyKey, TaskId};
use rusqlite::{Connection, TransactionBehavior};
use sha2::{Digest, Sha256};

use support::{READ_NOW_MS, TestStoreDir, artifact_id, artifact_spec, bytes, provenance_triple};

const CHILD_SCENARIO_ENV: &str = "NLOS_ARTIFACT_U12_SCENARIO";
const CHILD_ROOT_ENV: &str = "NLOS_ARTIFACT_U12_ROOT";
const CHILD_ARG_ENV: &str = "NLOS_ARTIFACT_U12_ARG";
const CHILD_STOP_ENV: &str = "NLOS_ARTIFACT_U12_STOP";
const STEP_TIMEOUT: Duration = Duration::from_secs(30);
const BARRIER_HOLD_MS: u64 = 250;

// ---------------------------------------------------------------------------
// Cross-platform child process harness with bounded timeouts and RAII cleanup
// ---------------------------------------------------------------------------

struct ManagedChild {
    name: String,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    lines: Receiver<Result<String, String>>,
}

impl ManagedChild {
    fn spawn(
        name: impl Into<String>,
        scenario: &str,
        root: &Path,
        arg: Option<&str>,
        stop_file: Option<&Path>,
    ) -> Self {
        let name = name.into();
        let mut command =
            Command::new(std::env::current_exe().expect("locate current test executable"));
        command
            .args([
                "--exact",
                "u12_cross_process_child_helper",
                "--ignored",
                "--nocapture",
            ])
            .env(CHILD_SCENARIO_ENV, scenario)
            .env(CHILD_ROOT_ENV, root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if let Some(arg_value) = arg {
            command.env(CHILD_ARG_ENV, arg_value);
        }
        if let Some(stop_path) = stop_file {
            command.env(CHILD_STOP_ENV, stop_path);
        }

        let mut child = command
            .spawn()
            .unwrap_or_else(|error| panic!("spawn child `{name}` ({scenario}): {error}"));
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("piped child stdout");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(text) => {
                        if tx.send(Ok(text)).is_err() {
                            return;
                        }
                    }
                    Err(error) => {
                        let _ = tx.send(Err(error.to_string()));
                        return;
                    }
                }
            }
        });

        Self {
            name,
            child: Some(child),
            stdin,
            lines: rx,
        }
    }

    fn await_marker(&self, prefix: &str, timeout: Duration) -> String {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "timed out waiting for marker `{prefix}` from child `{}`",
                self.name
            );
            match self.lines.recv_timeout(remaining) {
                Ok(Ok(line)) => {
                    if let Some(rest) = line.strip_prefix(prefix) {
                        return rest.to_string();
                    }
                }
                Ok(Err(error)) => {
                    panic!("stdout read error from child `{}`: {error}", self.name);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    panic!(
                        "timed out waiting for marker `{prefix}` from child `{}`",
                        self.name
                    );
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    panic!(
                        "child `{}` closed stdout before emitting `{prefix}`",
                        self.name
                    );
                }
            }
        }
    }

    fn send_line(&mut self, line: &str) {
        let stdin = self
            .stdin
            .as_mut()
            .unwrap_or_else(|| panic!("stdin already closed for child `{}`", self.name));
        writeln!(stdin, "{line}")
            .and_then(|()| stdin.flush())
            .unwrap_or_else(|error| panic!("write `{line}` to child `{}`: {error}", self.name));
    }

    fn assert_still_running(&mut self) {
        let child = self
            .child
            .as_mut()
            .unwrap_or_else(|| panic!("child `{}` already reaped", self.name));
        match child.try_wait() {
            Ok(None) => {}
            Ok(Some(status)) => panic!(
                "child `{}` exited prematurely while expected to block (status: {status})",
                self.name
            ),
            Err(error) => panic!("try_wait failed on child `{}`: {error}", self.name),
        }
    }

    fn wait_success(&mut self, timeout: Duration) -> Vec<String> {
        drop(self.stdin.take());
        let mut child = self
            .child
            .take()
            .unwrap_or_else(|| panic!("child `{}` already reaped", self.name));
        let deadline = Instant::now() + timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        panic!("timed out waiting for child `{}` to exit", self.name);
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("try_wait failed for child `{}`: {error}", self.name);
                }
            }
        };
        assert!(
            status.success(),
            "child `{}` exited with failure status {status}",
            self.name
        );

        let mut collected = Vec::new();
        while let Ok(Ok(line)) = self.lines.try_recv() {
            if line.starts_with("U12:") {
                collected.push(line);
            }
        }
        collected
    }

    fn kill_and_reap(&mut self) {
        drop(self.stdin.take());
        let mut child = self
            .child
            .take()
            .unwrap_or_else(|| panic!("child `{}` already reaped", self.name));
        child
            .kill()
            .unwrap_or_else(|error| panic!("kill child `{}`: {error}", self.name));
        let status = child
            .wait()
            .unwrap_or_else(|error| panic!("wait killed child `{}`: {error}", self.name));
        assert!(
            !status.success(),
            "killed child `{}` unexpectedly exited with success {status}",
            self.name
        );
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        drop(self.stdin.take());
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

struct StdinGate {
    rx: Receiver<Result<String, String>>,
}

impl StdinGate {
    fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let stdin = std::io::stdin();
            let reader = BufReader::new(stdin.lock());
            for line in reader.lines() {
                match line {
                    Ok(text) => {
                        if tx.send(Ok(text.trim().to_string())).is_err() {
                            return;
                        }
                    }
                    Err(error) => {
                        let _ = tx.send(Err(error.to_string()));
                        return;
                    }
                }
            }
        });
        Self { rx }
    }

    fn await_line(&self, expected: &str) {
        match self.rx.recv_timeout(STEP_TIMEOUT) {
            Ok(Ok(received)) => {
                assert_eq!(
                    received, expected,
                    "unexpected parent signal: expected `{expected}`, got `{received}`"
                );
            }
            Ok(Err(error)) => panic!("child failed reading parent signal: {error}"),
            Err(error) => panic!("child timed out waiting for `{expected}`: {error}"),
        }
    }
}

fn emit(marker: &str) {
    println!("{marker}");
    std::io::stdout().flush().expect("flush child marker");
}

// ---------------------------------------------------------------------------
// Deterministic test payloads and store helpers
// ---------------------------------------------------------------------------

fn payload_for(tag: u8, seq: u64) -> Vec<u8> {
    let len = 256 + usize::try_from(seq % 13).unwrap_or(0) * 193;
    let mut out = bytes(tag.wrapping_add(u8::try_from(seq & 0xff).unwrap_or(0)), len);
    let seq_bytes = seq.to_le_bytes();
    out[..8].copy_from_slice(&seq_bytes);
    out
}

fn put_req(artifact: ArtifactId, expected_head: u64, payload: &[u8]) -> PutRevisionRequest<'_> {
    let seed = artifact.as_bytes()[0].wrapping_add(u8::try_from(expected_head & 0xff).unwrap_or(0));
    PutRevisionRequest {
        artifact_id: artifact,
        expected_head_revision: expected_head,
        bytes: payload,
        created_at_ms: 5_000 + expected_head,
        provenance: provenance_triple(seed),
    }
}

fn stage_key(seed: u8, seq: u64) -> IdempotencyKey {
    let mut raw = [seed; 16];
    raw[8..].copy_from_slice(&seq.to_be_bytes());
    IdempotencyKey::from_bytes(raw)
}

fn stage_req(
    artifact: ArtifactId,
    expected_head: u64,
    payload: &[u8],
    key: IdempotencyKey,
) -> StageRevisionRequest<'_> {
    let seed = artifact.as_bytes()[0];
    StageRevisionRequest {
        artifact_id: artifact,
        expected_head_revision: expected_head,
        bytes: payload,
        task_id: TaskId::from_bytes([seed.wrapping_add(0x20); 16]),
        permit_id: CommitPermitId::from_bytes([seed.wrapping_add(0x40); 16]),
        write_set_root: ContentDigest::from_bytes([seed.wrapping_add(0x60); 32]),
        idempotency_key: key,
        created_at_ms: 6_000 + expected_head,
    }
}

fn publish_req(
    artifact: ArtifactId,
    staging_id: nlos_artifact::StagingId,
    seq: u64,
) -> PublishStagedRevisionRequest {
    let seed = artifact.as_bytes()[0];
    PublishStagedRevisionRequest {
        staging_id,
        task_id: TaskId::from_bytes([seed.wrapping_add(0x20); 16]),
        permit_id: CommitPermitId::from_bytes([seed.wrapping_add(0x40); 16]),
        write_set_root: ContentDigest::from_bytes([seed.wrapping_add(0x60); 32]),
        published_at_ms: 7_000 + seq,
    }
}

fn plant_orphan_atomic(root: &Path, tag: u8, seq: u64) -> ContentDigest {
    let payload = payload_for(tag, seq);
    let digest = ContentDigest::of_bytes(&payload);
    let hex = digest.to_hex();
    let shard = root.join("artifacts/blobs").join(&hex[..2]);
    fs::create_dir_all(&shard).expect("create orphan shard dir");
    let final_path = shard.join(&hex);
    if !final_path.exists() {
        let tmp_dir = root.join("orphan_plant_tmp");
        fs::create_dir_all(&tmp_dir).expect("create tmp staging dir for orphan plant");
        let tmp = tmp_dir.join(format!("{tag}-{seq}-{}.tmp", std::process::id()));
        {
            let mut file = fs::File::create(&tmp).expect("create orphan tmp");
            file.write_all(&payload).expect("write orphan tmp");
            file.sync_all().expect("sync orphan tmp");
        }
        fs::rename(&tmp, &final_path).expect("rename planted orphan into blobs");
    }
    digest
}

fn blob_path(root: &Path, digest: ContentDigest) -> PathBuf {
    let hex = digest.to_hex();
    root.join("artifacts/blobs").join(&hex[..2]).join(hex)
}

fn assert_integrity(root: &Path) {
    let connection = Connection::open(root.join("metadata.db")).expect("open for integrity_check");
    let result: String = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .expect("run PRAGMA integrity_check");
    assert_eq!(result, "ok", "SQLite integrity_check failed: {result}");
}

fn verify_revision_bytes(
    store: &ArtifactStore,
    artifact: ArtifactId,
    revision: u64,
    expected: &[u8],
) {
    let actual = store
        .get_revision(artifact, revision, READ_NOW_MS)
        .unwrap_or_else(|error| {
            panic!("get_revision({artifact:?}, rev={revision}) failed: {error}")
        });
    assert_eq!(
        ContentDigest::of_bytes(&actual),
        ContentDigest::of_bytes(expected),
        "SHA-256 mismatch on {artifact:?} rev {revision}"
    );
    assert_eq!(
        actual, expected,
        "byte payload mismatch on {artifact:?} rev {revision}"
    );
}

fn auto_gc_key_for_pass(pass_index: u64) -> IdempotencyKey {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/artifact-auto-gc-key/v1");
    hasher.update(pass_index.to_be_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    IdempotencyKey::from_bytes(bytes)
}

fn parse_env_u64(raw: &str, name: &str) -> u64 {
    raw.parse::<u64>()
        .unwrap_or_else(|error| panic!("invalid u64 for {name} (`{raw}`): {error}"))
}

fn parse_env_u8_hex(raw: &str, name: &str) -> u8 {
    u8::from_str_radix(raw, 16)
        .unwrap_or_else(|error| panic!("invalid hex u8 for {name} (`{raw}`): {error}"))
}

// ---------------------------------------------------------------------------
// Child worker entry point (invoked via `--exact ... --ignored`)
// ---------------------------------------------------------------------------

#[test]
#[ignore = "child process worker spawned by u12_cross_process_gc integration tests"]
#[allow(clippy::too_many_lines)]
fn u12_cross_process_child_helper() {
    let (Ok(scenario), Ok(root_raw)) = (
        std::env::var(CHILD_SCENARIO_ENV),
        std::env::var(CHILD_ROOT_ENV),
    ) else {
        eprintln!("skipped: {CHILD_SCENARIO_ENV}/{CHILD_ROOT_ENV} unset (parent spawn only)");
        return;
    };
    let root = PathBuf::from(root_raw);
    let arg = std::env::var(CHILD_ARG_ENV).ok();
    let stop_file = std::env::var(CHILD_STOP_ENV).ok().map(PathBuf::from);
    let gate = StdinGate::new();

    match scenario.as_str() {
        "hold-immediate-lock" => {
            let mut connection = Connection::open(root.join("metadata.db")).expect("open raw db");
            connection
                .busy_timeout(Duration::from_secs(5))
                .expect("busy timeout");
            let tx = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .expect("acquire BEGIN IMMEDIATE");
            let rev_count: i64 = tx
                .query_row("SELECT COUNT(*) FROM artifact_revisions", [], |r| r.get(0))
                .expect("count revisions");
            emit(&format!("U12:LOCKED:{rev_count}"));
            gate.await_line("GO");
            if let Some(hex) = arg {
                let digest = ContentDigest::from_hex(&hex).expect("hex digest arg");
                assert!(
                    !blob_path(&root, digest).exists(),
                    "in-flight blob must never hit disk while external BEGIN IMMEDIATE is held"
                );
            }
            tx.commit().expect("release BEGIN IMMEDIATE");
            emit("U12:UNLOCKED");
        }
        "put-revision-probe" => {
            let store = ArtifactStore::open(&root).expect("open store");
            let payload = payload_for(0x21, 0);
            emit("U12:READY");
            let decision = store
                .put_revision(put_req(artifact_id(0x01), 0, &payload))
                .expect("put_revision must commit once SQLite lock releases");
            assert!(matches!(decision, PutRevisionDecision::Committed(_)));
            emit("U12:DONE");
        }
        "stage-revision-probe" => {
            let store = ArtifactStore::open(&root).expect("open store");
            let payload = payload_for(0x22, 1);
            emit("U12:READY");
            let decision = store
                .stage_revision(stage_req(
                    artifact_id(0x01),
                    1,
                    &payload,
                    stage_key(0x22, 1),
                ))
                .expect("stage_revision must commit once SQLite lock releases");
            assert!(matches!(decision, StageRevisionDecision::Staged(_)));
            emit("U12:DONE");
        }
        "hold-blob-then-commit-rev3" => {
            let mut connection = Connection::open(root.join("metadata.db")).expect("open raw db");
            connection
                .busy_timeout(Duration::from_secs(5))
                .expect("busy timeout");
            let tx = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .expect("BEGIN IMMEDIATE");
            let payload = payload_for(0x23, 2);
            let digest = ContentDigest::of_bytes(&payload);
            let target_path = blob_path(&root, digest);
            fs::create_dir_all(target_path.parent().expect("shard")).expect("create shard");
            {
                let mut f = fs::File::create(&target_path).expect("create blob");
                f.write_all(&payload).expect("write blob");
                f.sync_all().expect("sync blob");
            }
            emit(&format!("U12:BLOB_WRITTEN:{}", digest.to_hex()));
            gate.await_line("COMMIT");
            let aid = artifact_id(0x01);
            tx.execute(
                "INSERT INTO artifact_revisions (artifact_id, revision, digest, size_bytes, created_at_ms)
                 VALUES (?1, 3, ?2, ?3, 5002)",
                rusqlite::params![
                    aid.as_bytes().as_slice(),
                    digest.as_bytes().as_slice(),
                    i64::try_from(payload.len()).expect("len fits i64"),
                ],
            )
            .expect("insert revision 3");
            tx.execute(
                "UPDATE artifacts SET head_revision = 3, head_digest = ?1 WHERE artifact_id = ?2",
                rusqlite::params![digest.as_bytes().as_slice(), aid.as_bytes().as_slice()],
            )
            .expect("update head 3");
            let mut hasher = Sha256::new();
            hasher.update(b"llmos/artifact-provenance-receipt/v1");
            hasher.update(aid.as_bytes());
            hasher.update(3_u64.to_be_bytes());
            let prov_hash = hasher.finalize();
            tx.execute(
                "INSERT INTO artifact_provenance_receipts (
                    receipt_id, artifact_id, revision, source_kind,
                    source_a, source_b, source_digest, publication_receipt_id, created_at_ms
                 ) VALUES (?1, ?2, 3, 0, ?3, ?4, ?5, NULL, 5002)",
                rusqlite::params![
                    &prov_hash[..16],
                    aid.as_bytes().as_slice(),
                    [0xaa_u8; 16].as_slice(),
                    [0xbb_u8; 16].as_slice(),
                    digest.as_bytes().as_slice(),
                ],
            )
            .expect("insert provenance 3");
            tx.commit().expect("commit rev 3");
            emit("U12:COMMITTED");
        }
        "gc-probe-single" => {
            let store = ArtifactStore::open(&root).expect("open store");
            let key_seed = parse_env_u8_hex(arg.as_deref().unwrap_or("91"), "key_seed");
            emit("U12:GC_READY");
            let decision = store
                .collect_orphan_blobs(CollectOrphanBlobsRequest {
                    idempotency_key: IdempotencyKey::from_bytes([key_seed; 16]),
                    collected_at_ms: 9_500,
                })
                .expect("collect_orphan_blobs");
            emit(&format!(
                "U12:GC_DONE:{}",
                decision.receipt().collected_count
            ));
        }
        "lockstep-writer" => {
            let rounds = parse_env_u64(arg.as_deref().unwrap_or("20"), "rounds");
            let store = ArtifactStore::open(&root).expect("open store");
            let aid = artifact_id(0x02);
            for round in 0..rounds {
                emit(&format!("U12:W_READY:{round}"));
                gate.await_line("GO");
                let payload = payload_for(0x30, round);
                if round % 2 == 0 {
                    let dec = store
                        .put_revision(put_req(aid, round, &payload))
                        .unwrap_or_else(|e| panic!("lockstep put round {round}: {e}"));
                    assert!(matches!(dec, PutRevisionDecision::Committed(_)));
                } else {
                    let key = stage_key(0x30, round);
                    let st = store
                        .stage_revision(stage_req(aid, round, &payload, key))
                        .unwrap_or_else(|e| panic!("lockstep stage round {round}: {e}"));
                    assert!(matches!(st, StageRevisionDecision::Staged(_)));
                    let pub_dec = store
                        .publish_staged_revision(publish_req(aid, st.record().staging_id, round))
                        .unwrap_or_else(|e| panic!("lockstep publish round {round}: {e}"));
                    assert!(matches!(
                        pub_dec,
                        PublishStagedRevisionDecision::Published(_)
                    ));
                }
                verify_revision_bytes(&store, aid, round + 1, &payload);
                emit(&format!("U12:W_DONE:{round}"));
            }
        }
        "lockstep-gc" => {
            let rounds = parse_env_u64(arg.as_deref().unwrap_or("20"), "rounds");
            let store = ArtifactStore::open(&root).expect("open store");
            for round in 0..rounds {
                emit(&format!("U12:G_READY:{round}"));
                gate.await_line("GO");
                let dec = store
                    .collect_orphan_blobs(CollectOrphanBlobsRequest {
                        idempotency_key: stage_key(0x70, round),
                        collected_at_ms: 10_000 + round,
                    })
                    .unwrap_or_else(|e| panic!("lockstep gc round {round}: {e}"));
                assert_eq!(
                    dec.receipt().collected_count,
                    0,
                    "lockstep gc round {round} falsely collected {:?}",
                    dec.receipt().collected_digests
                );
                emit(&format!("U12:G_DONE:{round}"));
            }
        }
        "hammer-put" => {
            let seed = parse_env_u8_hex(arg.as_deref().expect("seed arg"), "seed");
            let aid = artifact_id(seed);
            let store = ArtifactStore::open(&root).expect("open store");
            emit("U12:READY");
            gate.await_line("GO");
            let count = 16_u64;
            for rev in 0..count {
                let payload = payload_for(seed, rev);
                let dec = store
                    .put_revision(put_req(aid, rev, &payload))
                    .unwrap_or_else(|e| panic!("hammer put {seed:#x} rev {rev}: {e}"));
                assert!(matches!(dec, PutRevisionDecision::Committed(_)));
                verify_revision_bytes(&store, aid, rev + 1, &payload);
            }
            emit(&format!("U12:PUT_DONE:{count}"));
        }
        "hammer-stage-publish" => {
            let store = ArtifactStore::open(&root).expect("open store");
            let aid = artifact_id(0x13);
            let held_aid = artifact_id(0x14);
            emit("U12:READY");
            gate.await_line("GO");
            let count = 12_u64;
            for rev in 0..count {
                let payload = payload_for(0x13, rev);
                let key = stage_key(0x13, rev);
                let staged = store
                    .stage_revision(stage_req(aid, rev, &payload, key))
                    .unwrap_or_else(|e| panic!("hammer stage rev {rev}: {e}"));
                assert!(matches!(staged, StageRevisionDecision::Staged(_)));
                let published = store
                    .publish_staged_revision(publish_req(aid, staged.record().staging_id, rev))
                    .unwrap_or_else(|e| panic!("hammer publish rev {rev}: {e}"));
                assert!(matches!(
                    published,
                    PublishStagedRevisionDecision::Published(_)
                ));
                verify_revision_bytes(&store, aid, rev + 1, &payload);

                if rev == 5 {
                    let held_payload = payload_for(0x14, 0);
                    let held = store
                        .stage_revision(stage_req(held_aid, 0, &held_payload, stage_key(0x14, 0)))
                        .expect("stage unreleased held revision");
                    assert!(matches!(held, StageRevisionDecision::Staged(_)));
                }
            }
            emit(&format!("U12:STAGE_DONE:{count}"));
        }
        "hammer-reader" => {
            let stop_path = stop_file.expect("stop file path");
            let store = ArtifactStore::open(&root).expect("open store");
            let artifacts: [u8; 3] = [0x11, 0x12, 0x13];
            emit("U12:READY");
            gate.await_line("GO");
            let mut verified_reads = 0_u64;
            let mut sweeps = 0_u64;
            loop {
                for seed in artifacts {
                    let aid = artifact_id(seed);
                    if let Some(head) = store
                        .resolve_head(aid, READ_NOW_MS)
                        .unwrap_or_else(|e| panic!("reader resolve_head {seed:#x}: {e}"))
                    {
                        for rev in 1..=head.revision {
                            let expected = payload_for(seed, rev - 1);
                            verify_revision_bytes(&store, aid, rev, &expected);
                            verified_reads += 1;
                        }
                    }
                }
                sweeps += 1;
                if sweeps >= 5 && stop_path.exists() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            emit(&format!("U12:READS:{verified_reads}"));
        }
        "hammer-manual-gc" => {
            let stop_path = stop_file.expect("stop file path");
            let store = ArtifactStore::open(&root).expect("open store");
            emit("U12:READY");
            gate.await_line("GO");
            let mut pass = 0_u64;
            let mut total_collected = 0_u64;
            loop {
                let dec = store
                    .collect_orphan_blobs(CollectOrphanBlobsRequest {
                        idempotency_key: stage_key(0x55, pass),
                        collected_at_ms: 20_000 + pass,
                    })
                    .unwrap_or_else(|e| panic!("manual gc pass {pass}: {e}"));
                let receipt = dec.receipt();
                assert!(matches!(dec, CollectOrphanBlobsDecision::Collected(_)));
                total_collected += receipt.collected_count;
                for digest in &receipt.collected_digests {
                    emit(&format!("U12:MANUAL_DIGEST:{}", digest.to_hex()));
                }
                pass += 1;
                if pass >= 4 && stop_path.exists() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(6));
            }
            emit(&format!("U12:MANUAL_DONE:{total_collected}:{pass}"));
        }
        "hammer-auto-gc" => {
            let stop_path = stop_file.expect("stop file path");
            let worker_tag = parse_env_u64(arg.as_deref().unwrap_or("0"), "worker_tag");
            let store = ArtifactStore::open(&root).expect("open store");
            emit("U12:READY");
            gate.await_line("GO");
            let mut prev_passes = 0_u64;
            let mut collected_decisions = 0_u64;
            let mut replayed_decisions = 0_u64;
            let mut skipped_behind_peer = 0_u64;
            let mut step = 0_u64;
            loop {
                let now_ms = 30_000 + step;
                let decision = store
                    .tick_auto_gc(TickAutoGcRequest {
                        now_ms,
                        policy: AutoOrphanGcPolicy::Enabled {
                            interval_ms: Some(0),
                            orphan_threshold: None,
                        },
                    })
                    .unwrap_or_else(|e| panic!("auto gc worker {worker_tag} step {step}: {e}"));
                match decision {
                    AutoGcTickDecision::Collected(_) => collected_decisions += 1,
                    AutoGcTickDecision::Replayed(_) => replayed_decisions += 1,
                    AutoGcTickDecision::Skipped(AutoGcSkipReason::IntervalNotElapsed {
                        last_pass_at_ms,
                        eligible_at_ms,
                    }) => {
                        assert_eq!(eligible_at_ms, last_pass_at_ms);
                        assert!(
                            last_pass_at_ms > now_ms,
                            "interval_ms=0 may only skip when a concurrent worker advanced last_pass_at_ms ({last_pass_at_ms}) past now_ms ({now_ms})"
                        );
                        skipped_behind_peer += 1;
                    }
                    AutoGcTickDecision::Skipped(reason) => {
                        panic!("unexpected skip reason for interval_ms=0: {reason:?}");
                    }
                }
                let health = store
                    .inspect_auto_gc_health()
                    .expect("inspect_auto_gc_health");
                assert!(
                    health.passes_completed >= prev_passes,
                    "passes_completed regressed from {prev_passes} to {}",
                    health.passes_completed
                );
                assert_eq!(health.failure_count, 0, "unexpected auto gc failure");
                prev_passes = health.passes_completed;
                if step == 0 {
                    emit("U12:AUTO_FIRST");
                }
                step += 1;
                if step >= 4 && stop_path.exists() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(4));
            }
            emit(&format!(
                "U12:AUTO_DONE:{collected_decisions}:{replayed_decisions}:{skipped_behind_peer}"
            ));
        }
        "crash-mid-write-residue" => {
            let store = ArtifactStore::open(&root).expect("open store");
            let aid = artifact_id(0x41);
            let p2 = payload_for(0x41, 1);
            store
                .put_revision(put_req(aid, 1, &p2))
                .expect("commit durable rev 2 before crash simulation");
            drop(store);

            let mut connection = Connection::open(root.join("metadata.db")).expect("open raw");
            connection
                .busy_timeout(Duration::from_secs(5))
                .expect("busy timeout");
            let tx = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .expect("BEGIN IMMEDIATE");
            let crash_orphan = payload_for(0x9a, 0);
            let crash_digest = ContentDigest::of_bytes(&crash_orphan);
            let crash_blob_path = blob_path(&root, crash_digest);
            fs::create_dir_all(crash_blob_path.parent().expect("shard")).expect("shard");
            {
                let mut f = fs::File::create(&crash_blob_path).expect("create crash orphan blob");
                f.write_all(&crash_orphan).expect("write crash orphan");
                f.sync_all().expect("sync crash orphan");
            }
            fs::write(
                root.join("artifacts/tmp/killed-writer.0.tmp"),
                b"half-written",
            )
            .expect("plant orphan tmp");
            tx.execute(
                "INSERT INTO artifact_revisions (artifact_id, revision, digest, size_bytes, created_at_ms)
                 VALUES (?1, 3, ?2, ?3, 9001)",
                rusqlite::params![
                    aid.as_bytes().as_slice(),
                    crash_digest.as_bytes().as_slice(),
                    i64::try_from(crash_orphan.len()).expect("len"),
                ],
            )
            .expect("dirty uncommitted rev 3");
            emit(&format!("U12:READY_TO_KILL:{}", crash_digest.to_hex()));
            loop {
                std::thread::park();
            }
        }
        "clean-orphans-and-verify" => {
            let expected_csv = arg.expect("expected orphan hex CSV");
            let mut expected_orphans: Vec<ContentDigest> = expected_csv
                .split(',')
                .filter(|s| !s.is_empty())
                .map(|hex| ContentDigest::from_hex(hex).expect("valid hex digest"))
                .collect();
            expected_orphans.sort();

            let store = ArtifactStore::open(&root).expect("open store");
            let aid_a = artifact_id(0x41);
            let aid_b = artifact_id(0x42);

            let head_a = store
                .resolve_head(aid_a, READ_NOW_MS)
                .expect("resolve A")
                .expect("head A");
            assert_eq!(head_a.revision, 2);

            let gc_key = IdempotencyKey::from_bytes([0xcc; 16]);
            let decision = store
                .collect_orphan_blobs(CollectOrphanBlobsRequest {
                    idempotency_key: gc_key,
                    collected_at_ms: 99_000,
                })
                .expect("collect orphans");
            let CollectOrphanBlobsDecision::Collected(receipt) = decision else {
                panic!("expected fresh Collected decision, got {decision:?}");
            };
            assert_eq!(receipt.collected_digests, expected_orphans);
            assert_eq!(
                receipt.collected_count,
                u64::try_from(expected_orphans.len()).expect("len fits u64")
            );
            assert_eq!(receipt.scanned_blob_count, 5);
            assert_eq!(
                store
                    .inspect_gc_receipt(receipt.receipt_id)
                    .expect("inspect gc receipt"),
                receipt
            );

            let replay = store
                .collect_orphan_blobs(CollectOrphanBlobsRequest {
                    idempotency_key: gc_key,
                    collected_at_ms: 99_001,
                })
                .expect("replay gc");
            assert_eq!(
                replay,
                CollectOrphanBlobsDecision::Replayed(receipt.clone())
            );

            verify_revision_bytes(&store, aid_a, 1, &payload_for(0x41, 0));
            verify_revision_bytes(&store, aid_a, 2, &payload_for(0x41, 1));

            let staged_key = stage_key(0x42, 0);
            let sid = staging_id_for(aid_b, staged_key);
            let published = store
                .publish_staged_revision(publish_req(aid_b, sid, 0))
                .expect("staged blob survived GC and publishes cleanly");
            assert!(matches!(
                published,
                PublishStagedRevisionDecision::Published(_)
            ));
            verify_revision_bytes(&store, aid_b, 1, &payload_for(0x42, 0));

            for orphan in &expected_orphans {
                assert!(
                    !blob_path(&root, *orphan).exists(),
                    "collected orphan {} must be removed",
                    orphan.to_hex()
                );
            }

            let report = store.recover().expect("recover");
            assert_eq!(report.missing_blobs, Vec::new());
            assert_eq!(report.missing_staged_blobs, Vec::new());
            assert_eq!(report.orphan_blobs, Vec::new());
            assert_eq!(report.removed_tmp_files, 1);

            drop(store);
            let raw = Connection::open(root.join("metadata.db")).expect("raw open");
            assert!(
                raw.execute(
                    "UPDATE artifact_gc_receipts SET collected_count = 0 WHERE receipt_id = ?1",
                    [receipt.receipt_id.as_bytes().as_slice()],
                )
                .is_err(),
                "GcReceipt must be immutable against UPDATE"
            );
            assert!(
                raw.execute(
                    "DELETE FROM artifact_gc_receipts WHERE receipt_id = ?1",
                    [receipt.receipt_id.as_bytes().as_slice()],
                )
                .is_err(),
                "GcReceipt must be immutable against DELETE"
            );
            assert_integrity(&root);
            emit(&format!("U12:CLEANED:{}", receipt.collected_count));
        }
        other => panic!("unknown u12 child scenario `{other}`"),
    }
}

// ---------------------------------------------------------------------------
// Case 1: Deterministic barrier adversarial reproduction across OS processes
// ---------------------------------------------------------------------------

fn run_lock_barrier_probe(
    root: &Path,
    probe_name: &str,
    probe_scenario: &str,
    expected_digest: ContentDigest,
    gc_key_seed: &str,
) {
    let hex = expected_digest.to_hex();
    let mut lock_holder = ManagedChild::spawn(
        format!("lock-holder-{probe_name}"),
        "hold-immediate-lock",
        root,
        Some(&hex),
        None,
    );
    lock_holder.await_marker("U12:LOCKED:", STEP_TIMEOUT);

    let mut writer = ManagedChild::spawn(probe_name, probe_scenario, root, None, None);
    writer.await_marker("U12:READY", STEP_TIMEOUT);

    // Hold SQLite BEGIN IMMEDIATE across the barrier window: the writer process
    // must stay blocked and must NOT place any blob or tmp file on disk before
    // acquiring BEGIN IMMEDIATE.
    std::thread::sleep(Duration::from_millis(BARRIER_HOLD_MS));
    writer.assert_still_running();
    assert!(
        !blob_path(root, expected_digest).exists(),
        "{probe_name}: blob materialized on disk before acquiring BEGIN IMMEDIATE"
    );
    assert_eq!(
        fs::read_dir(root.join("artifacts/tmp"))
            .expect("read tmp")
            .count(),
        0,
        "{probe_name}: tmp file created before acquiring BEGIN IMMEDIATE"
    );

    lock_holder.send_line("GO");
    lock_holder.await_marker("U12:UNLOCKED", STEP_TIMEOUT);
    lock_holder.wait_success(STEP_TIMEOUT);

    writer.await_marker("U12:DONE", STEP_TIMEOUT);
    writer.wait_success(STEP_TIMEOUT);

    let mut gc = ManagedChild::spawn(
        format!("gc-after-{probe_name}"),
        "gc-probe-single",
        root,
        Some(gc_key_seed),
        None,
    );
    gc.await_marker("U12:GC_READY", STEP_TIMEOUT);
    assert_eq!(gc.await_marker("U12:GC_DONE:", STEP_TIMEOUT), "0");
    gc.wait_success(STEP_TIMEOUT);
}

fn run_reverse_gc_barrier_probe(root: &Path, rev3_payload: &[u8]) {
    let rev3_digest = ContentDigest::of_bytes(rev3_payload);
    let mut writer = ManagedChild::spawn(
        "hold-blob-writer",
        "hold-blob-then-commit-rev3",
        root,
        None,
        None,
    );
    let written_hex = writer.await_marker("U12:BLOB_WRITTEN:", STEP_TIMEOUT);
    assert_eq!(written_hex, rev3_digest.to_hex());

    let mut gc = ManagedChild::spawn("gc-blocked", "gc-probe-single", root, Some("93"), None);
    gc.await_marker("U12:GC_READY", STEP_TIMEOUT);
    std::thread::sleep(Duration::from_millis(BARRIER_HOLD_MS));
    gc.assert_still_running();
    assert!(
        blob_path(root, rev3_digest).is_file(),
        "blob must remain intact while writer holds BEGIN IMMEDIATE"
    );

    writer.send_line("COMMIT");
    writer.await_marker("U12:COMMITTED", STEP_TIMEOUT);
    writer.wait_success(STEP_TIMEOUT);

    assert_eq!(gc.await_marker("U12:GC_DONE:", STEP_TIMEOUT), "0");
    gc.wait_success(STEP_TIMEOUT);
}

fn run_lockstep_race(root: &Path, rounds: u64) {
    let rounds_str = rounds.to_string();
    let mut writer = ManagedChild::spawn(
        "lockstep-writer",
        "lockstep-writer",
        root,
        Some(&rounds_str),
        None,
    );
    let mut gc = ManagedChild::spawn("lockstep-gc", "lockstep-gc", root, Some(&rounds_str), None);

    for round in 0..rounds {
        assert_eq!(
            writer.await_marker("U12:W_READY:", STEP_TIMEOUT),
            round.to_string()
        );
        assert_eq!(
            gc.await_marker("U12:G_READY:", STEP_TIMEOUT),
            round.to_string()
        );
        writer.send_line("GO");
        gc.send_line("GO");
        assert_eq!(
            writer.await_marker("U12:W_DONE:", STEP_TIMEOUT),
            round.to_string()
        );
        assert_eq!(
            gc.await_marker("U12:G_DONE:", STEP_TIMEOUT),
            round.to_string()
        );
    }

    writer.wait_success(STEP_TIMEOUT);
    gc.wait_success(STEP_TIMEOUT);
}

#[test]
fn u12_barrier_serializes_write_commit_window_against_cross_process_gc() {
    let directory = TestStoreDir::new("u12-barrier");
    let root = directory.root();
    {
        let store = ArtifactStore::open(root).expect("open store");
        store
            .create_artifact(artifact_spec(0x01))
            .expect("create A1");
        store
            .create_artifact(artifact_spec(0x02))
            .expect("create A2");
    }

    // 1A: `put_revision` holds BEGIN IMMEDIATE before writing blob to disk.
    let put_payload = payload_for(0x21, 0);
    run_lock_barrier_probe(
        root,
        "put-probe",
        "put-revision-probe",
        ContentDigest::of_bytes(&put_payload),
        "91",
    );

    // 1B: `stage_revision` holds BEGIN IMMEDIATE before writing blob to disk.
    let stage_payload = payload_for(0x22, 1);
    run_lock_barrier_probe(
        root,
        "stage-probe",
        "stage-revision-probe",
        ContentDigest::of_bytes(&stage_payload),
        "92",
    );
    {
        let store = ArtifactStore::open(root).expect("reopen to publish stage");
        let sid = staging_id_for(artifact_id(0x01), stage_key(0x22, 1));
        let dec = store
            .publish_staged_revision(publish_req(artifact_id(0x01), sid, 1))
            .expect("publish stage after barrier");
        assert!(matches!(dec, PublishStagedRevisionDecision::Published(_)));
        verify_revision_bytes(&store, artifact_id(0x01), 1, &put_payload);
        verify_revision_bytes(&store, artifact_id(0x01), 2, &stage_payload);
    }

    // 1C (reverse barrier): `collect_orphan_blobs` blocks while another OS
    // process holds BEGIN IMMEDIATE between blob creation and metadata commit.
    let rev3_payload = payload_for(0x23, 2);
    run_reverse_gc_barrier_probe(root, &rev3_payload);

    // 1D: Lockstep simultaneous barrier races between real ArtifactStore
    // writer and real ArtifactStore `collect_orphan_blobs` across two OS processes.
    let rounds = 20_u64;
    run_lockstep_race(root, rounds);

    let store = ArtifactStore::open(root).expect("final open");
    verify_revision_bytes(&store, artifact_id(0x01), 1, &put_payload);
    verify_revision_bytes(&store, artifact_id(0x01), 2, &stage_payload);
    verify_revision_bytes(&store, artifact_id(0x01), 3, &rev3_payload);
    for round in 0..rounds {
        verify_revision_bytes(
            &store,
            artifact_id(0x02),
            round + 1,
            &payload_for(0x30, round),
        );
    }
    let report = store.recover().expect("recover");
    assert_eq!(report.missing_blobs, Vec::new());
    assert_eq!(report.missing_staged_blobs, Vec::new());
    assert_eq!(report.orphan_blobs, Vec::new());
    assert_integrity(root);
}

// ---------------------------------------------------------------------------
// Case 2: Multi-OS-process concurrent hammering matrix
// ---------------------------------------------------------------------------

#[test]
#[allow(clippy::too_many_lines)]
fn u12_multi_process_hammer_matrix_preserves_all_revisions_and_auto_gc_accounting() {
    let directory = TestStoreDir::new("u12-hammer");
    let root = directory.root();
    let stop_file = root.join("hammer.stop");
    let mut expected_orphans = BTreeSet::new();

    {
        let store = ArtifactStore::open(root).expect("open store");
        for seed in [0x11_u8, 0x12, 0x13, 0x14] {
            store
                .create_artifact(artifact_spec(seed))
                .expect("create hammer artifact");
        }
        for seq in 0..8_u64 {
            expected_orphans.insert(plant_orphan_atomic(root, 0x80, seq));
        }
    }

    let mut put_1 = ManagedChild::spawn("hammer-put-1", "hammer-put", root, Some("11"), None);
    let mut put_2 = ManagedChild::spawn("hammer-put-2", "hammer-put", root, Some("12"), None);
    let mut stager = ManagedChild::spawn("hammer-stager", "hammer-stage-publish", root, None, None);
    let mut reader = ManagedChild::spawn(
        "hammer-reader",
        "hammer-reader",
        root,
        None,
        Some(&stop_file),
    );
    let mut manual_gc = ManagedChild::spawn(
        "hammer-manual-gc",
        "hammer-manual-gc",
        root,
        None,
        Some(&stop_file),
    );
    let mut auto_gc_1 = ManagedChild::spawn(
        "hammer-auto-gc-1",
        "hammer-auto-gc",
        root,
        Some("1"),
        Some(&stop_file),
    );
    let mut auto_gc_2 = ManagedChild::spawn(
        "hammer-auto-gc-2",
        "hammer-auto-gc",
        root,
        Some("2"),
        Some(&stop_file),
    );

    for child in [
        &put_1, &put_2, &stager, &reader, &manual_gc, &auto_gc_1, &auto_gc_2,
    ] {
        child.await_marker("U12:READY", STEP_TIMEOUT);
    }

    // Launch writers, reader, and both competing auto-GC workers together so
    // both auto-GC processes race on pass 0 with the 8 pre-planted orphans
    // while mutations and reads are actively running.
    put_1.send_line("GO");
    put_2.send_line("GO");
    stager.send_line("GO");
    reader.send_line("GO");
    auto_gc_1.send_line("GO");
    auto_gc_2.send_line("GO");

    auto_gc_1.await_marker("U12:AUTO_FIRST", STEP_TIMEOUT);
    auto_gc_2.await_marker("U12:AUTO_FIRST", STEP_TIMEOUT);
    for seq in 8..16_u64 {
        expected_orphans.insert(plant_orphan_atomic(root, 0x81, seq));
    }
    manual_gc.send_line("GO");

    put_1.wait_success(STEP_TIMEOUT);
    put_2.wait_success(STEP_TIMEOUT);
    stager.wait_success(STEP_TIMEOUT);

    // Let GC/reader observe the final state and mid-run orphans before stop.
    std::thread::sleep(Duration::from_millis(40));
    fs::write(&stop_file, b"stop").expect("write stop flag");

    let reader_lines = reader.wait_success(STEP_TIMEOUT);
    let manual_lines = manual_gc.wait_success(STEP_TIMEOUT);
    auto_gc_1.wait_success(STEP_TIMEOUT);
    auto_gc_2.wait_success(STEP_TIMEOUT);

    let reads: u64 = reader_lines
        .iter()
        .find_map(|line| line.strip_prefix("U12:READS:"))
        .expect("reader summary line")
        .parse()
        .expect("parse read count");
    assert!(reads > 0, "reader must complete verified reads");

    let manual_reported: u64 = manual_lines
        .iter()
        .find_map(|line| line.strip_prefix("U12:MANUAL_DONE:"))
        .and_then(|rest| rest.split(':').next())
        .expect("manual gc summary")
        .parse()
        .expect("parse manual collected count");

    // Final convergence check and end-to-end verification in parent.
    let store = ArtifactStore::open(root).expect("reopen after hammer");
    let final_tick = store
        .tick_auto_gc(TickAutoGcRequest {
            now_ms: 90_000,
            policy: AutoOrphanGcPolicy::Enabled {
                interval_ms: Some(0),
                orphan_threshold: None,
            },
        })
        .expect("final tick_auto_gc");
    assert!(!matches!(final_tick, AutoGcTickDecision::Skipped(_)));

    for rev in 0..16_u64 {
        verify_revision_bytes(&store, artifact_id(0x11), rev + 1, &payload_for(0x11, rev));
        verify_revision_bytes(&store, artifact_id(0x12), rev + 1, &payload_for(0x12, rev));
    }
    for rev in 0..12_u64 {
        verify_revision_bytes(&store, artifact_id(0x13), rev + 1, &payload_for(0x13, rev));
    }

    // Unreleased staged revision must survive concurrent GC and publish cleanly.
    let held_sid = staging_id_for(artifact_id(0x14), stage_key(0x14, 0));
    let held_pub = store
        .publish_staged_revision(publish_req(artifact_id(0x14), held_sid, 0))
        .expect("unreleased staged blob must survive hammer GC");
    assert!(matches!(
        held_pub,
        PublishStagedRevisionDecision::Published(_)
    ));
    verify_revision_bytes(&store, artifact_id(0x14), 1, &payload_for(0x14, 0));

    for orphan in &expected_orphans {
        assert!(
            !blob_path(root, *orphan).exists(),
            "planted orphan {} survived concurrent GC",
            orphan.to_hex()
        );
    }

    let report = store.recover().expect("final recover");
    assert_eq!(report.missing_blobs, Vec::new());
    assert_eq!(report.missing_staged_blobs, Vec::new());
    assert_eq!(report.orphan_blobs, Vec::new());

    let health = store.inspect_auto_gc_health().expect("inspect health");
    assert_eq!(health.failure_count, 0);
    assert!(
        health.passes_completed >= 2,
        "expected >=2 completed auto-GC passes, got {}",
        health.passes_completed
    );
    assert!(
        health.orphans_collected_total > 0,
        "expected auto-GC to collect real orphans during hammer"
    );
    drop(store);

    // Reconcile durable SQLite receipt ledger against AutoGcHealth and planted orphans.
    let raw = Connection::open(root.join("metadata.db")).expect("open raw db");
    let mut stmt = raw
        .prepare(
            "SELECT idempotency_key, collected_count, collected_digests
             FROM artifact_gc_receipts",
        )
        .expect("prepare receipt scan");
    let mut rows = stmt.query([]).expect("query receipts");

    let expected_auto_keys: BTreeSet<[u8; 16]> = (0..health.passes_completed)
        .map(|k| *auto_gc_key_for_pass(k).as_bytes())
        .collect();
    let mut seen_auto_keys = BTreeSet::new();
    let mut auto_receipt_collected_sum = 0_u64;
    let mut manual_receipt_collected_sum = 0_u64;
    let mut all_collected_digests = Vec::new();

    while let Some(row) = rows.next().expect("next receipt row") {
        let key_vec: Vec<u8> = row.get(0).expect("key bytes");
        let key_arr: [u8; 16] = key_vec.try_into().expect("16-byte idempotency key");
        let count_i64: i64 = row.get(1).expect("collected_count");
        let count = u64::try_from(count_i64).expect("non-negative count");
        let packed: Vec<u8> = row.get(2).expect("packed digests");
        assert!(packed.len().is_multiple_of(32));
        let digests: Vec<ContentDigest> = packed
            .as_chunks::<32>()
            .0
            .iter()
            .map(|chunk| ContentDigest::from_bytes(*chunk))
            .collect();
        assert_eq!(u64::try_from(digests.len()).expect("len"), count);
        all_collected_digests.extend(digests);

        if expected_auto_keys.contains(&key_arr) {
            assert!(
                seen_auto_keys.insert(key_arr),
                "duplicate auto-GC pass receipt"
            );
            auto_receipt_collected_sum += count;
        } else {
            assert_ne!(
                key_arr,
                *auto_gc_key_for_pass(health.passes_completed).as_bytes(),
                "unaccounted auto-GC pass beyond passes_completed"
            );
            manual_receipt_collected_sum += count;
        }
    }

    assert_eq!(
        seen_auto_keys, expected_auto_keys,
        "every auto pass 0..passes_completed must have exactly one receipt"
    );
    assert_eq!(
        auto_receipt_collected_sum, health.orphans_collected_total,
        "orphans_collected_total must match sum of auto-GC receipts"
    );
    assert_eq!(manual_receipt_collected_sum, manual_reported);
    assert_eq!(
        manual_receipt_collected_sum + health.orphans_collected_total,
        u64::try_from(expected_orphans.len()).expect("len"),
        "sum of manual + auto collections must equal planted orphans"
    );
    assert_eq!(
        all_collected_digests.len(),
        expected_orphans.len(),
        "each planted orphan must be collected exactly once (no double collection)"
    );
    let collected_set: BTreeSet<ContentDigest> = all_collected_digests.into_iter().collect();
    assert_eq!(
        collected_set, expected_orphans,
        "collected set must match planted orphans with zero false deletions"
    );
    assert_integrity(root);
}

// ---------------------------------------------------------------------------
// Case 3: Crash-residue orphan cleanup across OS processes
// ---------------------------------------------------------------------------

#[test]
fn u12_killed_writer_crash_residue_collected_with_immutable_receipt_and_zero_false_deletes() {
    let directory = TestStoreDir::new("u12-crash-residue");
    let root = directory.root();
    {
        let store = ArtifactStore::open(root).expect("open store");
        store
            .create_artifact(artifact_spec(0x41))
            .expect("create A");
        store
            .create_artifact(artifact_spec(0x42))
            .expect("create B");
        store
            .put_revision(put_req(artifact_id(0x41), 0, &payload_for(0x41, 0)))
            .expect("put rev 1");
        let staged = store
            .stage_revision(stage_req(
                artifact_id(0x42),
                0,
                &payload_for(0x42, 0),
                stage_key(0x42, 0),
            ))
            .expect("stage B");
        assert!(matches!(staged, StageRevisionDecision::Staged(_)));
    }

    let mut crashing_writer =
        ManagedChild::spawn("crash-writer", "crash-mid-write-residue", root, None, None);
    let crash_orphan_hex = crashing_writer.await_marker("U12:READY_TO_KILL:", STEP_TIMEOUT);
    crashing_writer.kill_and_reap();

    let extra_orphan = plant_orphan_atomic(root, 0x9b, 0);
    let expected_csv = format!("{crash_orphan_hex},{}", extra_orphan.to_hex());

    let mut cleaner = ManagedChild::spawn(
        "crash-cleaner",
        "clean-orphans-and-verify",
        root,
        Some(&expected_csv),
        None,
    );
    assert_eq!(cleaner.await_marker("U12:CLEANED:", STEP_TIMEOUT), "2");
    cleaner.wait_success(STEP_TIMEOUT);
}
