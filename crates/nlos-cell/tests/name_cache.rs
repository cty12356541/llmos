//! Capability/name cache: insert/get semantics, monotone invalidation,
//! generation fences, and epoch-wide fencing.
//!
//! All tests build fence snapshots directly (`CellFence::present`), so no
//! test claims the process-scoped `CellAuthority`.

use std::num::NonZeroU64;

use nlos_cell::{
    CacheHit, CachedCapability, CapabilityNameCache, CellEpoch, CellFence, CellFencingToken,
    CellIdentity, InsertOutcome, InvalidationOutcome, NameCacheError, NamePath,
};
use nlos_types::{CapabilityId, Generation, SchedulerDomainId};

fn fence(epoch: CellEpoch) -> CellFence {
    CellFence::present(
        CellIdentity::from_domain(SchedulerDomainId::from_bytes([0x9a; 16])),
        Generation::INITIAL,
        epoch,
        CellFencingToken::INITIAL,
    )
}

fn path(text: &str) -> NamePath {
    NamePath::new(text).expect("valid path")
}

fn capability(byte: u8) -> CapabilityId {
    CapabilityId::from_bytes([byte; 16])
}

fn generation(value: u64) -> Generation {
    Generation::new(NonZeroU64::new(value).expect("nonzero generation"))
}

#[test]
fn name_path_validates_absolute_printable_segments() {
    assert_eq!(path("/app").as_str(), "/app");
    assert_eq!(path("/app/win-1").as_str(), "/app/win-1");
    for bad in [
        "",
        "/",
        "/app/",
        "/app//win",
        "app/win",
        "/app/win ",
        "/äpp",
    ] {
        let error = NamePath::new(bad).expect_err("must reject");
        assert_eq!(
            error,
            NameCacheError::InvalidPath {
                presented: bad.to_string(),
            }
        );
    }
    // Ordering and hashing treat paths as their text.
    assert!(path("/a") < path("/b"));
}

#[test]
fn insert_get_replace_and_idempotent_replay() {
    let mut cache = CapabilityNameCache::new(&fence(CellEpoch::INITIAL));
    let name = path("/app/agent-1/session");
    assert_eq!(cache.get(&name), None, "miss is a plain None");

    assert_eq!(
        cache
            .insert(name.clone(), capability(0x01), generation(4))
            .expect("insert"),
        InsertOutcome::Inserted
    );
    assert_eq!(
        cache.get(&name),
        Some(CacheHit {
            cached: CachedCapability {
                capability: capability(0x01),
                capability_generation: generation(4),
            },
            inserted_epoch: CellEpoch::INITIAL,
        })
    );

    // Identical replay is idempotent.
    assert_eq!(
        cache
            .insert(name.clone(), capability(0x01), generation(4))
            .expect("replay"),
        InsertOutcome::Unchanged
    );

    // Newer generation replaces the older entry.
    assert_eq!(
        cache
            .insert(name.clone(), capability(0x02), generation(5))
            .expect("replace"),
        InsertOutcome::Replaced {
            previous: CachedCapability {
                capability: capability(0x01),
                capability_generation: generation(4),
            },
        }
    );
    assert_eq!(
        cache.get(&name).expect("hit").cached.capability,
        capability(0x02)
    );

    // Older generation than live is a typed stale reject.
    assert_eq!(
        cache
            .insert(name.clone(), capability(0x03), generation(4))
            .expect_err("stale entry"),
        NameCacheError::StaleEntry {
            path: name.clone(),
            presented: generation(4),
            current: generation(5),
        }
    );

    // Same generation, different capability is a conflict.
    assert_eq!(
        cache
            .insert(name.clone(), capability(0x03), generation(5))
            .expect_err("conflict"),
        NameCacheError::GenerationConflict {
            path: name.clone(),
            presented: capability(0x03),
            current: capability(0x02),
            generation: generation(5),
        }
    );
    assert_eq!(cache.path_count(), 1);
}

#[test]
fn invalidation_is_monotone_and_fences_generation_forever() {
    let mut cache = CapabilityNameCache::new(&fence(CellEpoch::INITIAL));
    let name = path("/artifact/scope");
    cache
        .insert(name.clone(), capability(0x10), generation(7))
        .expect("insert");

    // Invalidation through 5 does not touch the live gen-7 entry.
    assert_eq!(
        cache
            .invalidate(name.clone(), generation(5))
            .expect("advance"),
        InvalidationOutcome::Advanced {
            invalidated_through: generation(5),
            evicted: 0,
        }
    );
    assert_eq!(
        cache
            .get(&name)
            .expect("still live")
            .cached
            .capability_generation,
        generation(7)
    );

    // Re-invalidating below or at the high-water: below is a typed reject,
    // equal is idempotent.
    assert_eq!(
        cache
            .invalidate(name.clone(), generation(4))
            .expect_err("rollback"),
        NameCacheError::StaleInvalidation {
            path: name.clone(),
            presented: generation(4),
            invalidated_through: generation(5),
        }
    );
    assert_eq!(
        cache
            .invalidate(name.clone(), generation(5))
            .expect("replay"),
        InvalidationOutcome::Idempotent
    );

    // Invalidating through the live generation evicts it...
    assert_eq!(
        cache
            .invalidate(name.clone(), generation(7))
            .expect("evict"),
        InvalidationOutcome::Advanced {
            invalidated_through: generation(7),
            evicted: 1,
        }
    );
    assert_eq!(cache.get(&name), None);

    // ...and the fence is permanent: re-insert at or below the high-water
    // is rejected, only strictly newer generations may enter.
    assert_eq!(
        cache
            .insert(name.clone(), capability(0x11), generation(7))
            .expect_err("fenced insert"),
        NameCacheError::StaleGeneration {
            path: name.clone(),
            presented: generation(7),
            invalidated_through: generation(7),
        }
    );
    assert_eq!(
        cache
            .insert(name.clone(), capability(0x11), generation(6))
            .expect_err("fenced insert below"),
        NameCacheError::StaleGeneration {
            path: name.clone(),
            presented: generation(6),
            invalidated_through: generation(7),
        }
    );
    assert_eq!(
        cache
            .insert(name.clone(), capability(0x11), generation(8))
            .expect("newer generation re-enters"),
        InsertOutcome::Inserted
    );
    assert!(cache.get(&name).is_some());

    // Invalidating a never-seen path installs the fence pre-emptively.
    let unseen = path("/future/name");
    assert_eq!(
        cache
            .invalidate(unseen.clone(), generation(2))
            .expect("pre-fence"),
        InvalidationOutcome::Advanced {
            invalidated_through: generation(2),
            evicted: 0,
        }
    );
    assert_eq!(
        cache
            .insert(unseen.clone(), capability(0x12), generation(2))
            .expect_err("pre-fenced insert"),
        NameCacheError::StaleGeneration {
            path: unseen,
            presented: generation(2),
            invalidated_through: generation(2),
        }
    );
}

#[test]
fn epoch_advance_fences_all_entries_but_keeps_generation_high_waters() {
    let mut cache = CapabilityNameCache::new(&fence(CellEpoch::INITIAL));
    let live_name = path("/live/entry");
    let invalidated_name = path("/invalidated/entry");
    cache
        .insert(live_name.clone(), capability(0x20), generation(3))
        .expect("insert live");
    cache
        .insert(invalidated_name.clone(), capability(0x21), generation(3))
        .expect("insert then invalidate");
    cache
        .invalidate(invalidated_name.clone(), generation(3))
        .expect("invalidate");

    let epoch2 = CellEpoch::INITIAL.checked_next().expect("epoch 2");
    let report = cache
        .on_epoch_advanced(&fence(epoch2))
        .expect("advance epoch");
    assert_eq!(report.previous_epoch, CellEpoch::INITIAL);
    assert_eq!(report.new_epoch, epoch2);
    assert_eq!(
        report.entries_fenced, 1,
        "only the still-live entry was fenced"
    );
    assert_eq!(cache.epoch(), epoch2);

    // Old-epoch entry is invisible even though it was never explicitly
    // invalidated.
    assert_eq!(cache.get(&live_name), None);

    // Re-insertion in the new epoch works, but the old invalidation
    // high-water survives the boundary.
    assert_eq!(
        cache
            .insert(invalidated_name.clone(), capability(0x22), generation(3))
            .expect_err("high-water survives epoch"),
        NameCacheError::StaleGeneration {
            path: invalidated_name.clone(),
            presented: generation(3),
            invalidated_through: generation(3),
        }
    );
    assert_eq!(
        cache
            .insert(invalidated_name, capability(0x22), generation(4))
            .expect("newer generation after boundary"),
        InsertOutcome::Inserted
    );

    // The re-inserted entry is served under the new epoch.
    let hit = cache.get(&path("/invalidated/entry")).expect("hit");
    assert_eq!(hit.inserted_epoch, epoch2);
    assert_eq!(hit.cached.capability_generation, generation(4));
}

#[test]
fn epoch_advance_rejects_foreign_identity_and_non_advancing_fences() {
    let mut cache = CapabilityNameCache::new(&fence(CellEpoch::INITIAL));
    let epoch2 = CellEpoch::INITIAL.checked_next().expect("epoch 2");

    let foreign = CellFence::present(
        CellIdentity::from_domain(SchedulerDomainId::from_bytes([0xbb; 16])),
        Generation::INITIAL,
        epoch2,
        CellFencingToken::INITIAL,
    );
    assert_eq!(
        cache
            .on_epoch_advanced(&foreign)
            .expect_err("foreign identity"),
        NameCacheError::IdentityMismatch {
            presented: foreign.identity(),
            current: cache.identity(),
        }
    );
    assert_eq!(
        cache
            .on_epoch_advanced(&fence(CellEpoch::INITIAL))
            .expect_err("equal epoch"),
        NameCacheError::EpochNotAdvanced {
            presented: CellEpoch::INITIAL,
            current: CellEpoch::INITIAL,
        }
    );
    cache
        .on_epoch_advanced(&fence(epoch2))
        .expect("valid advance");
    assert_eq!(
        cache
            .on_epoch_advanced(&fence(epoch2))
            .expect_err("repeat advance"),
        NameCacheError::EpochNotAdvanced {
            presented: epoch2,
            current: epoch2,
        }
    );
    assert_eq!(
        cache
            .on_epoch_advanced(&fence(CellEpoch::INITIAL))
            .expect_err("older epoch"),
        NameCacheError::EpochNotAdvanced {
            presented: CellEpoch::INITIAL,
            current: epoch2,
        }
    );
}
