//! Sandbox profile: the declarative capability and hard-limit envelope that
//! governs every untrusted guest run (`ADR-0015` layer-2 caps + WASI
//! allowlist grants; `[TCB-BYPASS-001]`, `[TCB-EGRESS-001]`).
//!
//! The profile is deliberately deny-by-default: [`SandboxProfile::deny_all`]
//! grants no preopened directories, no guest arguments, no epoch deadline,
//! and a minimal fuel/memory budget. Every grant is additive and validated
//! fail-closed by [`SandboxProfile::validate`] before the engine is built.

use std::path::{Path, PathBuf};

use crate::error::ProfileInvalid;

/// Hard ceiling of a wasm32 linear memory in 64 KiB pages (4 GiB).
pub const MAX_WASM_MEMORY_PAGES: u64 = 65_536;

/// Size of one WebAssembly linear-memory page in bytes.
pub const WASM_PAGE_BYTES: u64 = 65_536;

/// A single filesystem capability grant: one host directory exposed to the
/// guest under one guest-facing name (`[TCB-EGRESS-001]` egress scope).
///
/// The host root is canonicalized at construction time so that containment
/// checks inside the WASI allowlist surface always compare against a fully
/// resolved absolute path.
#[derive(Debug, Clone)]
pub struct PreopenGrant {
    guest_dir: String,
    host_root: PathBuf,
    allow_write: bool,
}

impl PreopenGrant {
    /// Creates a grant, validating the guest-facing name and resolving the
    /// host root. `guest_dir` must be a guest-absolute path such as
    /// `/workspace` whose components are all ordinary names; `host_root`
    /// must be an existing directory.
    ///
    /// # Errors
    ///
    /// Returns the concrete [`ProfileInvalid`] violation when the guest
    /// name is malformed or the host root cannot be resolved to an
    /// existing directory.
    pub fn new(
        guest_dir: &str,
        host_root: &Path,
        allow_write: bool,
    ) -> Result<Self, ProfileInvalid> {
        if guest_dir.is_empty() || !guest_dir.starts_with('/') {
            return Err(ProfileInvalid::PreopenGuestDirNotAbsolute {
                guest_dir: guest_dir.to_string(),
            });
        }
        if guest_dir.ends_with('/') || guest_dir.contains('\\') || guest_dir.contains('\0') {
            return Err(ProfileInvalid::PreopenGuestDirUnsafeComponent {
                guest_dir: guest_dir.to_string(),
            });
        }
        for component in guest_dir.split('/').skip(1) {
            if component.is_empty() || component == "." || component == ".." {
                return Err(ProfileInvalid::PreopenGuestDirUnsafeComponent {
                    guest_dir: guest_dir.to_string(),
                });
            }
        }
        let canonical_root = std::fs::canonicalize(host_root).map_err(|_| {
            ProfileInvalid::PreopenHostRootInvalid {
                host_root: host_root.to_path_buf(),
            }
        })?;
        if !canonical_root.is_dir() {
            return Err(ProfileInvalid::PreopenHostRootInvalid {
                host_root: host_root.to_path_buf(),
            });
        }
        Ok(Self {
            guest_dir: guest_dir.to_string(),
            host_root: canonical_root,
            allow_write,
        })
    }

    /// The guest-facing mount name (for example `/workspace`).
    #[must_use]
    pub fn guest_dir(&self) -> &str {
        &self.guest_dir
    }

    /// The canonicalized host-side directory backing this grant.
    #[must_use]
    pub fn host_root(&self) -> &Path {
        &self.host_root
    }

    /// Whether write-intent opens inside this grant are permitted.
    #[must_use]
    pub fn allow_write(&self) -> bool {
        self.allow_write
    }
}

/// The complete capability and hard-limit envelope for one guest run.
///
/// Defaults are deny-all: no preopens, no guest arguments, no epoch
/// deadline, `DEFAULT_FUEL` fuel, and a small memory page cap. The profile
/// is plain data so it can cross the process boundary as explicit argv.
#[derive(Debug, Clone)]
pub struct SandboxProfile {
    fuel: u64,
    max_memory_pages: u64,
    epoch_deadline_ticks: u64,
    epoch_tick_interval_ms: u64,
    preopens: Vec<PreopenGrant>,
    guest_args: Vec<String>,
}

impl Default for SandboxProfile {
    fn default() -> Self {
        Self::deny_all()
    }
}

impl SandboxProfile {
    /// Default fuel budget for a deny-all run (about 10^6 engine
    /// operations; sufficient for tiny probe guests, far below anything
    /// that could spin meaningfully).
    pub const DEFAULT_FUEL: u64 = 1_000_000;

    /// Default linear-memory cap: 16 pages = 1 MiB.
    pub const DEFAULT_MEMORY_PAGES: u64 = 16;

    /// Deny-all baseline profile: empty preopen set, empty guest argv, no
    /// epoch deadline, minimal deterministic fuel and memory caps.
    #[must_use]
    pub fn deny_all() -> Self {
        Self {
            fuel: Self::DEFAULT_FUEL,
            max_memory_pages: Self::DEFAULT_MEMORY_PAGES,
            epoch_deadline_ticks: 0,
            epoch_tick_interval_ms: 0,
            preopens: Vec::new(),
            guest_args: Vec::new(),
        }
    }

    /// Sets the deterministic CPU fuel quota (must be `>= 1`).
    #[must_use]
    pub fn with_fuel(mut self, fuel: u64) -> Self {
        self.fuel = fuel;
        self
    }

    /// Sets the linear-memory cap in 64 KiB pages (must be `1..=65536`).
    #[must_use]
    pub fn with_max_memory_pages(mut self, pages: u64) -> Self {
        self.max_memory_pages = pages;
        self
    }

    /// Enables epoch interruption with the given deadline (in ticks beyond
    /// the epoch at run start) and ticker interval. A `ticks` value of `0`
    /// disables epoch interruption.
    #[must_use]
    pub fn with_epoch_deadline(mut self, ticks: u64, tick_interval_ms: u64) -> Self {
        self.epoch_deadline_ticks = ticks;
        self.epoch_tick_interval_ms = tick_interval_ms;
        self
    }

    /// Adds one preopen grant.
    #[must_use]
    pub fn with_preopen(mut self, grant: PreopenGrant) -> Self {
        self.preopens.push(grant);
        self
    }

    /// Replaces the explicit guest argv (the only arguments a guest can
    /// ever observe; the host environment contributes nothing).
    #[must_use]
    pub fn with_guest_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.guest_args = args.into_iter().map(Into::into).collect();
        self
    }

    /// Fail-closed structural validation. Returns `Err` on the first
    /// violated invariant; the engine layer refuses to run unvalidated
    /// profiles.
    ///
    /// # Errors
    ///
    /// Returns the concrete [`ProfileInvalid`] violation.
    pub fn validate(&self) -> Result<(), ProfileInvalid> {
        if self.fuel == 0 {
            return Err(ProfileInvalid::FuelZero);
        }
        if self.max_memory_pages == 0 || self.max_memory_pages > MAX_WASM_MEMORY_PAGES {
            return Err(ProfileInvalid::MemoryPagesOutOfRange {
                pages: self.max_memory_pages,
            });
        }
        if self.epoch_deadline_ticks > 0 && self.epoch_tick_interval_ms == 0 {
            // A deadline without a ticker interval can never fire; reject it
            // rather than silently degrading the wall-clock guarantee.
            return Err(ProfileInvalid::EpochIntervalMissing);
        }
        if self.guest_args.iter().any(|a| a.contains('\0')) {
            return Err(ProfileInvalid::EmbeddedNul {
                field: "guest argv",
            });
        }
        Ok(())
    }

    /// The deterministic CPU fuel quota.
    #[must_use]
    pub fn fuel(&self) -> u64 {
        self.fuel
    }

    /// The linear-memory cap in 64 KiB pages.
    #[must_use]
    pub fn max_memory_pages(&self) -> u64 {
        self.max_memory_pages
    }

    /// The epoch deadline in ticks beyond run start (`0` = disabled).
    #[must_use]
    pub fn epoch_deadline_ticks(&self) -> u64 {
        self.epoch_deadline_ticks
    }

    /// The epoch ticker interval in milliseconds.
    #[must_use]
    pub fn epoch_tick_interval_ms(&self) -> u64 {
        self.epoch_tick_interval_ms
    }

    /// The granted preopen set (empty by default).
    #[must_use]
    pub fn preopens(&self) -> &[PreopenGrant] {
        &self.preopens
    }

    /// The explicit guest argv (empty by default).
    #[must_use]
    pub fn guest_args(&self) -> &[String] {
        &self.guest_args
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_root(tag: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "nlos-e6a-profile-{tag}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    #[test]
    fn deny_all_defaults_grant_nothing() {
        let profile = SandboxProfile::deny_all();
        assert_eq!(profile.preopens().len(), 0);
        assert_eq!(profile.guest_args().len(), 0);
        assert_eq!(profile.epoch_deadline_ticks(), 0);
        assert!(profile.fuel() >= 1);
        assert!(profile.validate().is_ok());
    }

    #[test]
    fn zero_fuel_is_rejected_fail_closed() {
        let err = SandboxProfile::deny_all().with_fuel(0).validate();
        assert!(matches!(err, Err(ProfileInvalid::FuelZero)));
    }

    #[test]
    fn memory_pages_outside_one_to_65536_are_rejected() {
        let zero = SandboxProfile::deny_all()
            .with_max_memory_pages(0)
            .validate();
        assert!(matches!(
            zero,
            Err(ProfileInvalid::MemoryPagesOutOfRange { pages: 0 })
        ));
        let huge = SandboxProfile::deny_all()
            .with_max_memory_pages(65_537)
            .validate();
        assert!(matches!(
            huge,
            Err(ProfileInvalid::MemoryPagesOutOfRange { pages: 65_537 })
        ));
        let ok = SandboxProfile::deny_all()
            .with_max_memory_pages(65_536)
            .validate();
        assert!(ok.is_ok());
    }

    #[test]
    fn epoch_deadline_without_ticker_interval_is_rejected() {
        let err = SandboxProfile::deny_all()
            .with_epoch_deadline(10, 0)
            .validate();
        assert!(err.is_err(), "deadline without interval must not validate");
    }

    #[test]
    fn preopen_guest_dir_names_are_strictly_guest_absolute() {
        let root = scratch_root("names");
        for bad in [
            "workspace",
            "",
            "/a/",
            "/a//b",
            "/a/./b",
            "/a/../b",
            "/a\\b",
        ] {
            let err = PreopenGrant::new(bad, &root, false);
            assert!(err.is_err(), "guest dir {bad:?} must be rejected");
        }
        let ok = PreopenGrant::new("/workspace", &root, false).expect("valid grant");
        assert_eq!(ok.guest_dir(), "/workspace");
        assert!(!ok.allow_write());
    }

    #[test]
    fn preopen_host_root_must_be_existing_directory() {
        let root = scratch_root("hostroot");
        let file = root.join("plain.txt");
        std::fs::write(&file, b"x").expect("write file");
        assert!(matches!(
            PreopenGrant::new("/f", &root.join("missing"), false),
            Err(ProfileInvalid::PreopenHostRootInvalid { .. })
        ));
        assert!(matches!(
            PreopenGrant::new("/f", &file, false),
            Err(ProfileInvalid::PreopenHostRootInvalid { .. })
        ));
        assert!(PreopenGrant::new("/d", &root, true).is_ok());
    }

    #[test]
    fn guest_args_with_nul_are_rejected() {
        let err = SandboxProfile::deny_all()
            .with_guest_args(["alpha\0evil"])
            .validate();
        assert!(matches!(
            err,
            Err(ProfileInvalid::EmbeddedNul {
                field: "guest argv"
            })
        ));
    }
}
