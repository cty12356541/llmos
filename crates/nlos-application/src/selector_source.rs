//! Ecosystem selector source adapter for the plan resolver's
//! `EcosystemEntityKind::Application` half (W36-P7): maps this
//! authority's generation-carrying readback
//! (`ApplicationAuthority::inspect_application`) onto
//! `nlos_plan::EcosystemSelectorSource`. The dependency direction is
//! the existing one — this crate already depends on `nlos-plan` — so
//! the adapter is a thin extension beside the entity it exposes.

use nlos_plan::{
    EcosystemEntityKind, EcosystemEntityState, EcosystemSelector, EcosystemSelectorSource,
};

use crate::{ApplicationAuthority, ApplicationAuthorityError};

/// [`EcosystemSelectorSource`] over one application authority: the
/// pinned generation is the application's current installation
/// generation, the content anchor is the installed package's manifest
/// digest. The readback is status-independent (a disabled or
/// uninstalled application keeps its generation history — the pinned
/// identity stays observable).
pub struct ApplicationSelectorSource<'a> {
    authority: &'a ApplicationAuthority,
}

impl<'a> ApplicationSelectorSource<'a> {
    #[must_use]
    pub const fn new(authority: &'a ApplicationAuthority) -> Self {
        Self { authority }
    }
}

/// The adapter's failure surface: the application authority's own
/// error, or the contract violation of being consulted for a foreign
/// kind ([`EcosystemSelectorSource::kinds`] gates this away in
/// practice).
#[derive(Debug)]
pub enum ApplicationSourceError {
    Authority(ApplicationAuthorityError),
    UnsupportedKind(EcosystemEntityKind),
}

impl std::fmt::Display for ApplicationSourceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Authority(error) => {
                write!(formatter, "application authority lookup failed: {error}")
            }
            Self::UnsupportedKind(kind) => write!(
                formatter,
                "application source was consulted for foreign kind {kind:?}"
            ),
        }
    }
}

impl std::error::Error for ApplicationSourceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Authority(error) => Some(error),
            Self::UnsupportedKind(_) => None,
        }
    }
}

impl From<ApplicationAuthorityError> for ApplicationSourceError {
    fn from(error: ApplicationAuthorityError) -> Self {
        Self::Authority(error)
    }
}

impl EcosystemSelectorSource for ApplicationSelectorSource<'_> {
    type Error = ApplicationSourceError;

    fn kinds(&self) -> &'static [EcosystemEntityKind] {
        &[EcosystemEntityKind::Application]
    }

    fn lookup(
        &self,
        selector: &EcosystemSelector,
    ) -> Result<nlos_plan::EcosystemSourceLookup, Self::Error> {
        let package_id = match selector {
            EcosystemSelector::Application { package_id, .. } => *package_id,
            other @ EcosystemSelector::Artifact { .. } => {
                return Err(ApplicationSourceError::UnsupportedKind(other.kind()));
            }
        };
        match self.authority.inspect_application(package_id)? {
            Some(view) => Ok(nlos_plan::EcosystemSourceLookup::Found(
                EcosystemEntityState {
                    generation: view.current_installation_generation.get(),
                    content_digest: view.package_manifest_digest.into_bytes(),
                },
            )),
            None => Ok(nlos_plan::EcosystemSourceLookup::NotFound),
        }
    }
}
