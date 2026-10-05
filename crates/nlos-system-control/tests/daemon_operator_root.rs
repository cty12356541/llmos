//! ADR-0022 operator root-issuance tests (`daemon` feature): the
//! fail-closed default (unconfigured means no issuance step and no
//! capability authority at all), the configured path (a real
//! `issue_root_signed` production call whose receipt is queryable through a
//! fresh authority handle), restart idempotency (same key + same parameters
//! replays the original receipt), the parameter-scoped replay boundary (a
//! changed purpose digest issues a distinct root, never an
//! `IdempotencyConflict`), and the typed key-file contract rejections
//! (absent file, malformed seed, and — on Unix — group/other permission
//! bits).

#![cfg(feature = "daemon")]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use ed25519_dalek::SigningKey;
use nlos_capability::{CapabilityAuthority, CapabilityRights, CapabilityTarget};
use nlos_system_control::daemon::{DaemonError, DaemonOptions, OperatorKeyFileError, assemble};
use nlos_types::NamespaceId;
use rusqlite::Connection;

static NEXT_PATH: AtomicU64 = AtomicU64::new(0);

/// Short-lived temp state root, mirroring the assembly-test harness (the
/// label stays short: the default endpoint paths derive from it and Unix
/// socket paths must fit the `SUN_LEN` bound).
struct TempRoot(PathBuf);

impl TempRoot {
    fn new(label: &str) -> Self {
        let sequence = NEXT_PATH.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!(
            "nlos-dop-{label}-{}-{sequence}",
            std::process::id(),
        )))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Short explicit endpoint path (host OS form), so a long state-root label
/// cannot push the derived default past the Unix `SUN_LEN` bound.
#[cfg(unix)]
fn temp_socket(label: &str) -> PathBuf {
    let sequence = NEXT_PATH.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "nlos-dop-{label}-{}-{sequence}.sock",
        std::process::id(),
    ))
}

/// Short explicit endpoint path (host OS form), so a long state-root label
/// cannot push the derived default past the Unix `SUN_LEN` bound.
#[cfg(windows)]
fn temp_socket(label: &str) -> PathBuf {
    let sequence = NEXT_PATH.fetch_add(1, Ordering::Relaxed);
    PathBuf::from(format!(
        r"\\.\pipe\nlos-dop-{label}-{}-{sequence}",
        std::process::id(),
    ))
}

/// Daemon options over `root` with both endpoints pinned to short explicit
/// paths (see [`temp_socket`]).
fn options_with_sockets(root: &Path, label: &str) -> DaemonOptions {
    DaemonOptions::new(root)
        .with_auth_socket(temp_socket(&format!("{label}-a")))
        .with_plain_socket(temp_socket(&format!("{label}-p")))
}

/// Writes one operator key file in the desktop-client format (trimmed
/// 64-hex Ed25519 seed) under owner-only `0600` permission bits.
struct TempKeyFile {
    path: PathBuf,
}

impl TempKeyFile {
    fn new(label: &str, seed: u8) -> Self {
        let sequence = NEXT_PATH.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "nlos-daemon-operator-key-{label}-{}-{sequence}.key",
            std::process::id(),
        ));
        let key = SigningKey::from_bytes(&[seed; 32]);
        fs::write(&path, hex(key.verifying_key().as_bytes())).expect("write key file");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("chmod 0600");
        }
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempKeyFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn capability_database(root: &Path) -> PathBuf {
    root.join("capability").join("capability-authority.db")
}

fn capability_heads_count(root: &Path) -> i64 {
    Connection::open(capability_database(root))
        .expect("open capability authority database")
        .query_row("SELECT COUNT(*) FROM capability_heads", [], |row| {
            row.get(0)
        })
        .expect("count capability heads")
}

/// Unconfigured means zero issuance and no capability face at all: the
/// assembled daemon reports no issuance, holds no capability authority, and
/// the state root carries no capability database (the pre-ADR-0022 layout,
/// byte for byte).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unconfigured_daemon_runs_no_issuance_step() {
    let root = TempRoot::new("unconfigured");
    let (daemon, endpoints) = assemble(
        options_with_sockets(root.path(), "off"),
        tokio::runtime::Handle::current(),
    )
    .expect("assemble daemon");

    assert!(daemon.operator_root.is_none(), "no issuance reported");
    assert!(daemon.capabilities.is_none(), "no capability authority");
    assert!(
        !capability_database(root.path()).exists(),
        "no capability database under the state root"
    );

    daemon.stop_worker();
    daemon.stop_materialization();
    drop(endpoints);
}

/// The configured path issues one root capability through the real
/// signature-gated authority API, binds the operator principal as issuer
/// and holder over the root namespace with the conservative defaults, and
/// leaves the receipt queryable through a fresh authority handle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn configured_operator_key_issues_a_queryable_root() {
    let root = TempRoot::new("configured");
    let key = TempKeyFile::new("configured", 0x11);
    let (daemon, endpoints) = assemble(
        options_with_sockets(root.path(), "on").with_operator_key_file(key.path()),
        tokio::runtime::Handle::current(),
    )
    .expect("assemble daemon");

    let issuance = daemon.operator_root.as_ref().expect("issuance ran");
    assert!(!issuance.replayed, "first start issues, not replays");

    // The root binds the operator principal as both issuer and holder,
    // over the all-zero root namespace, with the conservative defaults.
    let record = issuance.record;
    assert_eq!(record.issuer, record.holder);
    assert_eq!(
        record.target,
        CapabilityTarget::Namespace(NamespaceId::from_bytes([0; 16]))
    );
    assert!(record.rights.contains(CapabilityRights::SEMANTIC_APPEND));
    assert!(record.rights.contains(CapabilityRights::SEMANTIC_RETRACT));
    assert!(
        record
            .rights
            .contains(CapabilityRights::SEMANTIC_ADJUDICATE)
    );
    assert!(record.rights.contains(CapabilityRights::DELEGATE));
    assert_eq!(record.delegation_depth_remaining, 3);
    assert_eq!(record.call_limit, None);
    assert!(record.parent.is_none());
    assert!(record.revoked_at_ms.is_none());
    assert!(record.valid_from_ms <= record.valid_until_ms);

    // The receipt is queryable through a fresh authority handle over the
    // durable store: the exact generation is active and the durable record
    // matches the in-memory issuance.
    let authority = CapabilityAuthority::open(root.path().join("capability"))
        .expect("open capability authority");
    let durable = authority
        .inspect_active(record.handle, 1)
        .expect("issued root is queryable and active");
    assert_eq!(durable, record);
    assert_eq!(capability_heads_count(root.path()), 1);

    daemon.stop_worker();
    daemon.stop_materialization();
    drop(endpoints);
}

/// Restarting with the same key and the same parameters replays the
/// original receipt (no second root), while a changed purpose digest is a
/// different issuance identity and issues a distinct root — the replay
/// boundary is the (key, parameters) pair, never a startup brick.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_replays_the_same_receipt_and_scopes_the_replay_to_parameters() {
    let root = TempRoot::new("replay");
    let key = TempKeyFile::new("replay", 0x22);

    let (first, endpoints) = assemble(
        options_with_sockets(root.path(), "r1").with_operator_key_file(key.path()),
        tokio::runtime::Handle::current(),
    )
    .expect("first assemble");
    let first_issuance = first.operator_root.as_ref().expect("first issuance");
    assert!(!first_issuance.replayed);
    let first_receipt = first_issuance.receipt;
    let first_capability = first_issuance.record.handle.capability_id;
    first.stop_worker();
    first.stop_materialization();
    drop(first);
    drop(endpoints);

    // Same key + same parameters: the original receipt replays verbatim.
    let (second, endpoints) = assemble(
        options_with_sockets(root.path(), "r2").with_operator_key_file(key.path()),
        tokio::runtime::Handle::current(),
    )
    .expect("restart assemble");
    let replay = second.operator_root.as_ref().expect("replay issuance");
    assert!(replay.replayed, "restart replays the original decision");
    assert_eq!(replay.receipt.receipt_id, first_receipt.receipt_id);
    assert_eq!(replay.receipt.capability_id, first_capability);
    assert_eq!(replay.receipt.issued_at_ms, first_receipt.issued_at_ms);
    assert_eq!(capability_heads_count(root.path()), 1);
    second.stop_worker();
    second.stop_materialization();
    drop(second);
    drop(endpoints);

    // Same key + a different purpose digest: a distinct issuance identity
    // issues a second root instead of colliding on the old digest.
    let (third, endpoints) = assemble(
        options_with_sockets(root.path(), "r3")
            .with_operator_key_file(key.path())
            .with_operator_purpose_digest(Some([0x5a; 32])),
        tokio::runtime::Handle::current(),
    )
    .expect("parameterized assemble");
    let purposeful = third.operator_root.as_ref().expect("purposeful issuance");
    assert!(!purposeful.replayed, "changed parameters issue, not replay");
    assert_ne!(
        purposeful.record.handle.capability_id, first_capability,
        "a distinct root per issuance identity"
    );
    assert_eq!(purposeful.record.purpose_digest, Some([0x5a; 32]));
    assert_eq!(capability_heads_count(root.path()), 2);
    third.stop_worker();
    third.stop_materialization();
    drop(endpoints);
}

/// The key-file contract fails closed with typed rejections: an absent
/// file, a malformed seed, and — on Unix — group/other permission bits.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bad_key_files_are_typed_rejected() {
    let root = TempRoot::new("rejected");

    // Absent file.
    let Err(absent) = assemble(
        DaemonOptions::new(root.path()).with_operator_key_file(root.path().join("absent.key")),
        tokio::runtime::Handle::current(),
    ) else {
        panic!("absent key file must fail closed")
    };
    assert!(
        matches!(
            absent,
            DaemonError::OperatorKeyFile(OperatorKeyFileError::Unreadable)
        ),
        "absent file: {absent}"
    );

    // Malformed seed (desktop-client 64-hex contract).
    let malformed = root.path().join("malformed.key");
    fs::write(&malformed, "not-a-seed").expect("write malformed key file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(&malformed, fs::Permissions::from_mode(0o600)).expect("chmod 0600");
    }
    let Err(rejected) = assemble(
        DaemonOptions::new(root.path()).with_operator_key_file(&malformed),
        tokio::runtime::Handle::current(),
    ) else {
        panic!("malformed key file must fail closed")
    };
    assert!(
        matches!(
            rejected,
            DaemonError::OperatorKeyFile(OperatorKeyFileError::NotHexSeed)
        ),
        "malformed seed: {rejected}"
    );

    // Insecure permission bits (Unix-only contract arm).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let insecure = TempKeyFile::new("insecure", 0x33);
        fs::set_permissions(insecure.path(), fs::Permissions::from_mode(0o644))
            .expect("chmod 0644");
        let Err(rejected) = assemble(
            DaemonOptions::new(root.path()).with_operator_key_file(insecure.path()),
            tokio::runtime::Handle::current(),
        ) else {
            panic!("group-readable key file must fail closed")
        };
        assert!(
            matches!(
                rejected,
                DaemonError::OperatorKeyFile(OperatorKeyFileError::InsecureMode(0o644))
            ),
            "insecure mode: {rejected}"
        );
    }
}
