//! Domain-separated `ReceiptId` derivation for the authority-backed
//! operation executors (W29-D).
//!
//! Every executor receipt id is SHA-256 over a per-authority domain label
//! plus the typed facts the backing authority call produced — the same
//! derive-then-truncate scheme `nlos-process` uses for authority-assigned
//! identities. The id therefore proves the executor reached its authority:
//! it cannot be produced without the authority's receipt facts.

use nlos_types::ReceiptId;
use sha2::{Digest, Sha256};

/// Hashes `domain` and every part (length-free concatenation; the domains
/// pin their own part shapes) and truncates to the 16-byte `ReceiptId`.
pub(crate) fn derive_executor_receipt_id(domain: &[u8], parts: &[&[u8]]) -> ReceiptId {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    for part in parts {
        hasher.update(part);
    }
    let digest = hasher.finalize();
    ReceiptId::from_bytes(digest[..16].try_into().expect("sha256 digest is 32 bytes"))
}
