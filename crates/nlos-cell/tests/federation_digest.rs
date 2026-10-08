//! CS0-B: the structured checkpoint-digest face — determinism, fail-closed
//! verification, and the unchanged-storage round trip.
//!
//! ADR-0019 R1.2.5 registers a checkpoint as an *observation face* claim:
//! the old authority's stated durable prefix. This file pins the CS0-B
//! half of that claim's semantics: [`nlos_cell::CheckpointDigest`] derives
//! a domain-separated `SHA-256` over the fence axes (boot / epoch / token)
//! plus a canonical, length-prefixed prefix serialization, and
//! [`nlos_cell::CheckpointFact::verify_digest`] recomputes and compares it
//! against a claimed prefix, returning a typed outcome where mismatch and
//! corruption are values, never silent passes (fail-closed, the module's
//! own discipline). Consuming a verified checkpoint inside a
//! reconciliation executor is CS0-C and is deliberately not claimed here.
//!
//! The digest's canonical text form (`v1:sha256:` + 64 lowercase hex,
//! 74 bytes) must keep fitting the fact's existing ≤256-byte single-line
//! digest axis: the storage format does not change to carry one.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_cell::{
    CellDirectory, CellEpoch, CellFencingToken, CellIdentity, CheckpointDigest,
    CheckpointDigestVerification, CheckpointFact, CheckpointPrefix,
};
use nlos_types::{Generation, SchedulerDomainId};

struct TempDir {
    root: PathBuf,
}

impl TempDir {
    fn new(name: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "nlos-cell-federation-digest-{name}-{}-{sequence}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create federation digest temp root");
        Self { root }
    }

    fn path(&self) -> &Path {
        &self.root
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        match fs::remove_dir_all(&self.root) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("remove federation digest temp root: {error}"),
        }
    }
}

fn identity(byte: u8) -> CellIdentity {
    CellIdentity::from_domain(SchedulerDomainId::from_bytes([byte; 16]))
}

fn structured_fact(text: &str) -> CheckpointFact {
    CheckpointFact::new(
        Generation::INITIAL,
        CellEpoch::INITIAL,
        CellFencingToken::INITIAL,
        text,
    )
    .expect("valid checkpoint fact")
}

#[test]
fn digest_is_deterministic_and_pins_every_axis_and_prefix_byte() {
    let prefix = CheckpointPrefix::from_entries(["commit-a".as_bytes(), "commit-b".as_bytes()]);
    let boot = Generation::INITIAL;
    let epoch = CellEpoch::INITIAL;
    let token = CellFencingToken::INITIAL;

    // Deterministic: same axes, same prefix, same digest — bytes and text.
    let first = CheckpointDigest::from_prefix(boot, epoch, token, &prefix);
    let second = CheckpointDigest::from_prefix(boot, epoch, token, &prefix);
    assert_eq!(first, second);
    assert_eq!(first.as_bytes(), second.as_bytes());
    assert_eq!(first.to_text(), second.to_text());
    let text = first.to_text();
    assert!(
        text.starts_with("v1:sha256:") && text.len() == 74,
        "canonical text form is tag + 64 lowercase hex"
    );

    // Each fence axis is pinned: moving any one changes the digest.
    let moved_boot = CheckpointDigest::from_prefix(
        Generation::INITIAL.checked_next().expect("boot 2"),
        epoch,
        token,
        &prefix,
    );
    let moved_epoch = CheckpointDigest::from_prefix(
        boot,
        CellEpoch::INITIAL.checked_next().expect("epoch 2"),
        token,
        &prefix,
    );
    let moved_token = CheckpointDigest::from_prefix(
        boot,
        epoch,
        CellFencingToken::INITIAL.checked_next().expect("token 2"),
        &prefix,
    );
    assert_ne!(first, moved_boot, "boot axis is hashed in");
    assert_ne!(first, moved_epoch, "epoch axis is hashed in");
    assert_ne!(first, moved_token, "token axis is hashed in");

    // Every prefix byte is pinned: one flipped byte, one appended entry,
    // one inserted entry all change the digest.
    let flipped = CheckpointPrefix::from_entries(["commit-A".as_bytes(), "commit-b".as_bytes()]);
    let appended = CheckpointPrefix::from_entries([
        "commit-a".as_bytes(),
        "commit-b".as_bytes(),
        "commit-c".as_bytes(),
    ]);
    let inserted = CheckpointPrefix::from_entries([
        "commit-0".as_bytes(),
        "commit-a".as_bytes(),
        "commit-b".as_bytes(),
    ]);
    for variant in [flipped, appended, inserted] {
        assert_ne!(
            first,
            CheckpointDigest::from_prefix(boot, epoch, token, &variant),
            "prefix change must change the digest"
        );
    }

    // The length-prefixed grammar keeps distinct entry sequences apart:
    // [ab, c] is not [abc], and "empty prefix" is not "one empty entry".
    let split = CheckpointDigest::from_prefix(
        boot,
        epoch,
        token,
        &CheckpointPrefix::from_entries(["ab".as_bytes(), "c".as_bytes()]),
    );
    let joined = CheckpointDigest::from_prefix(
        boot,
        epoch,
        token,
        &CheckpointPrefix::from_entries(["abc".as_bytes()]),
    );
    assert_ne!(split, joined, "entry boundaries are length-pinned");
    let none = CheckpointDigest::from_prefix(boot, epoch, token, &CheckpointPrefix::empty());
    let one_empty = CheckpointDigest::from_prefix(
        boot,
        epoch,
        token,
        &CheckpointPrefix::from_entries(["".as_bytes()]),
    );
    assert_ne!(none, one_empty, "count axis is length-pinned");

    // The empty prefix is still a first-class (deterministic) claim.
    assert_eq!(
        none,
        CheckpointDigest::from_prefix(boot, epoch, token, &CheckpointPrefix::empty())
    );
}

#[test]
fn verifier_matches_the_authors_own_digest() {
    let prefix =
        CheckpointPrefix::from_entries(["durable-prefix-entry-1".as_bytes(), "entry-2".as_bytes()]);
    let digest = CheckpointDigest::from_prefix(
        Generation::INITIAL,
        CellEpoch::INITIAL,
        CellFencingToken::INITIAL,
        &prefix,
    );
    let fact = structured_fact(&digest.to_text());

    // The author's own claim verifies against the very prefix it was
    // computed over — including a freshly rebuilt, equal prefix.
    assert_eq!(
        fact.verify_digest(&prefix),
        CheckpointDigestVerification::Verified
    );
    let rebuilt =
        CheckpointPrefix::from_entries(["durable-prefix-entry-1".as_bytes(), "entry-2".as_bytes()]);
    assert_eq!(
        fact.verify_digest(&rebuilt),
        CheckpointDigestVerification::Verified
    );

    // The accessor round trip: entries survive construction in order.
    assert_eq!(prefix.entries(), rebuilt.entries());
}

#[test]
fn verifier_fails_closed_on_mismatch_tampering_and_axis_moves() {
    let prefix = CheckpointPrefix::from_entries(["entry-1".as_bytes(), "entry-2".as_bytes()]);
    let digest = CheckpointDigest::from_prefix(
        Generation::INITIAL,
        CellEpoch::INITIAL,
        CellFencingToken::INITIAL,
        &prefix,
    );
    let author = structured_fact(&digest.to_text());

    // A different prefix under the same fact: mismatch, with both digests
    // presented for audit — never a silent pass.
    let other_prefix =
        CheckpointPrefix::from_entries(["entry-1".as_bytes(), "entry-2/tampered".as_bytes()]);
    assert_eq!(
        author.verify_digest(&other_prefix),
        CheckpointDigestVerification::Mismatch {
            presented: digest,
            recomputed: CheckpointDigest::from_prefix(
                Generation::INITIAL,
                CellEpoch::INITIAL,
                CellFencingToken::INITIAL,
                &other_prefix,
            ),
        }
    );

    // A well-formed digest spelling a different claim (the author's text
    // replaced wholesale): mismatch, fail closed.
    let other_claim = CheckpointDigest::from_prefix(
        Generation::INITIAL,
        CellEpoch::INITIAL,
        CellFencingToken::INITIAL,
        &CheckpointPrefix::from_entries(["not-the-claimed-prefix".as_bytes()]),
    );
    let foreign_fact = structured_fact(&other_claim.to_text());
    assert_eq!(
        foreign_fact.verify_digest(&prefix),
        CheckpointDigestVerification::Mismatch {
            presented: other_claim,
            recomputed: digest,
        }
    );

    // One flipped hex character is still a well-formed spelling of some
    // other digest: still a mismatch, never a pass.
    let mut flipped = digest.to_text();
    let replacement = if flipped.ends_with('0') { '1' } else { '0' };
    flipped.pop();
    flipped.push(replacement);
    let flipped_fact = structured_fact(&flipped);
    assert!(matches!(
        flipped_fact.verify_digest(&prefix),
        CheckpointDigestVerification::Mismatch { .. }
    ));

    // Fence axes are pinned into the digest: the same digest text under a
    // moved epoch no longer matches the same prefix.
    let moved_epoch_fact = CheckpointFact::new(
        Generation::INITIAL,
        CellEpoch::INITIAL.checked_next().expect("epoch 2"),
        CellFencingToken::INITIAL,
        &digest.to_text(),
    )
    .expect("valid fact with moved epoch");
    assert_eq!(
        moved_epoch_fact.verify_digest(&prefix),
        CheckpointDigestVerification::Mismatch {
            presented: digest,
            recomputed: CheckpointDigest::from_prefix(
                Generation::INITIAL,
                CellEpoch::INITIAL.checked_next().expect("epoch 2"),
                CellFencingToken::INITIAL,
                &prefix,
            ),
        }
    );
}

#[test]
fn verifier_separates_free_text_from_corrupt_bodies() {
    let prefix = CheckpointPrefix::from_entries(["entry-1".as_bytes()]);

    // Author free text (the pre-CS0-B digest axis): honestly not
    // comparable — neither verified nor corrupt.
    assert_eq!(
        structured_fact("quota high-water 7/10").verify_digest(&prefix),
        CheckpointDigestVerification::FreeText
    );

    // A tagged body that does not decode: fail closed as corrupt, never
    // re-read as free text and never as a match.
    let sixty_three_hex = "0".repeat(63);
    let upper_hex = "AB".repeat(32);
    for broken in [
        "v1:sha256:".to_string(),
        format!("v1:sha256:{sixty_three_hex}"),
        format!("v1:sha256:{}0", "0".repeat(64)),
        format!("v1:sha256:{upper_hex}"),
        format!("v1:sha256:{}", "z".repeat(64)),
    ] {
        let fact = structured_fact(&broken);
        assert_eq!(
            fact.verify_digest(&prefix),
            CheckpointDigestVerification::Corrupt,
            "tagged but undecodable body must be corrupt: {broken:?}"
        );
    }
}

#[test]
fn digest_text_round_trips_through_the_checkpoint_log() {
    let root = TempDir::new("round-trip");
    let directory = CellDirectory::open(root.path()).expect("open directory");
    let cell = identity(0xe1);

    let prefix =
        CheckpointPrefix::from_entries(["commit-root-1".as_bytes(), "commit-root-2".as_bytes()]);
    let digest = CheckpointDigest::from_prefix(
        Generation::INITIAL,
        CellEpoch::INITIAL,
        CellFencingToken::INITIAL,
        &prefix,
    );

    // The canonical text keeps the fact's single-line ≤256-byte contract.
    let text = digest.to_text();
    assert!(text.len() <= 256, "fits the digest axis bound");
    assert!(!text.contains(['\n', '\r']), "single line");

    // Record and enumerate through the unchanged registration face.
    let fact = structured_fact(&text);
    let recorded = directory
        .record_checkpoint(cell, &fact)
        .expect("record structured checkpoint");
    let log = directory.checkpoints(cell).expect("enumerate");
    assert_eq!(log.corrupt(), 0);
    let records = log.into_records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0], recorded, "append order and full record survive");
    assert_eq!(records[0].fact().digest(), text);

    // The on-disk file still speaks the same key=value single-line format.
    let checkpoints_dir = root.path().join("checkpoints");
    let mut files: Vec<PathBuf> = fs::read_dir(&checkpoints_dir)
        .expect("list checkpoints dir")
        .map(|entry| entry.expect("dir entry").path())
        .collect();
    files.sort();
    assert_eq!(files.len(), 1, "exactly one checkpoint file");
    let raw = fs::read_to_string(&files[0]).expect("read staged checkpoint");
    assert!(raw.contains("digest=v1:sha256:"));
    assert_eq!(
        raw.lines().count(),
        8,
        "same eight-line staged record shape"
    );

    // The enumerated fact still verifies — and still fails closed against
    // a prefix it did not claim.
    assert_eq!(
        records[0].fact().verify_digest(&prefix),
        CheckpointDigestVerification::Verified
    );
    let wrong = CheckpointPrefix::from_entries([
        "commit-root-1".as_bytes(),
        "commit-root-2".as_bytes(),
        "extra".as_bytes(),
    ]);
    assert_eq!(
        records[0].fact().verify_digest(&wrong),
        CheckpointDigestVerification::Mismatch {
            presented: digest,
            recomputed: CheckpointDigest::from_prefix(
                Generation::INITIAL,
                CellEpoch::INITIAL,
                CellFencingToken::INITIAL,
                &wrong,
            ),
        }
    );
}
