//! Feature-gated ecosystem source adapter for `EcosystemEntityKind::Artifact`
//! (W36-P7, `artifact-source` feature): maps the artifact authority's
//! head readback onto [`EcosystemSelectorSource`]. The optional
//! dependency keeps the plan authority's default graph decoupled — this
//! adapter exists so assemblers can register the real surface without
//! the resolver ever naming the authority crate.

use nlos_artifact::{ArtifactError, ArtifactStore};

use crate::model::{
    EcosystemEntityKind, EcosystemEntityState, EcosystemSelector, EcosystemSourceLookup,
};
use crate::selector::EcosystemSelectorSource;

/// [`EcosystemSelectorSource`] over one artifact store's heads: the
/// pinned generation is the head revision, the content anchor is the
/// head revision's content digest. `now_ms` is the caller-supplied
/// observation time the store's retention checks consult (this crate
/// holds no clock).
pub struct ArtifactSelectorSource<'a> {
    store: &'a ArtifactStore,
    now_ms: u64,
}

impl<'a> ArtifactSelectorSource<'a> {
    #[must_use]
    pub const fn new(store: &'a ArtifactStore, now_ms: u64) -> Self {
        Self { store, now_ms }
    }
}

/// The adapter's failure surface: the artifact authority's own error,
/// or the contract violation of being consulted for a foreign kind
/// ([`EcosystemSelectorSource::kinds`] gates this away in practice).
#[derive(Debug)]
pub enum ArtifactSourceError {
    Store(ArtifactError),
    UnsupportedKind(EcosystemEntityKind),
}

impl std::fmt::Display for ArtifactSourceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(error) => write!(formatter, "artifact store lookup failed: {error}"),
            Self::UnsupportedKind(kind) => write!(
                formatter,
                "artifact source was consulted for foreign kind {kind:?}"
            ),
        }
    }
}

impl std::error::Error for ArtifactSourceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(error) => Some(error),
            Self::UnsupportedKind(_) => None,
        }
    }
}

impl From<ArtifactError> for ArtifactSourceError {
    fn from(error: ArtifactError) -> Self {
        Self::Store(error)
    }
}

impl EcosystemSelectorSource for ArtifactSelectorSource<'_> {
    type Error = ArtifactSourceError;

    fn kinds(&self) -> &'static [EcosystemEntityKind] {
        &[EcosystemEntityKind::Artifact]
    }

    fn lookup(&self, selector: &EcosystemSelector) -> Result<EcosystemSourceLookup, Self::Error> {
        let artifact_id = match selector {
            EcosystemSelector::Artifact { artifact_id, .. } => *artifact_id,
            other @ EcosystemSelector::Application { .. } => {
                return Err(ArtifactSourceError::UnsupportedKind(other.kind()));
            }
        };
        match self.store.resolve_head(artifact_id, self.now_ms) {
            Ok(Some(head)) => Ok(EcosystemSourceLookup::Found(EcosystemEntityState {
                generation: head.revision,
                content_digest: head.digest.into_bytes(),
            })),
            // An artifact with no revisions yet, or an unknown artifact
            // id: a typed miss — there is no generation to pin.
            Ok(None) | Err(ArtifactError::ArtifactNotFound(_)) => {
                Ok(EcosystemSourceLookup::NotFound)
            }
            Err(error) => Err(ArtifactSourceError::Store(error)),
        }
    }
}
