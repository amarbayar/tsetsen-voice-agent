//! Chooses which providers serve a request, per layer.
//!
//! A [`Router`] is built once from configuration: an ordered [`Route`] (primary,
//! then fallbacks) over registered providers. Per request it filters that order
//! by language support and egress policy, and reports every exclusion so the
//! caller can explain *why* a provider was skipped.
//!
//! Language rules need no special syntax: a provider that declares
//! `languages = ["en"]` is skipped for Mongolian, and the next one in the
//! route serves the request.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tsetsen_core::{Describe, LanguageTag, Layer, ProviderId, Sensitivity};
use tsetsen_policy::{EgressDenied, EgressPolicy};

/// Provider ids for one layer, in the order they should be tried.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Route {
    primary: ProviderId,
    fallbacks: Vec<ProviderId>,
}

impl Route {
    /// A route with only a primary provider.
    pub fn new(primary: impl Into<ProviderId>) -> Self {
        Self {
            primary: primary.into(),
            fallbacks: Vec::new(),
        }
    }

    /// Adds fallbacks, tried in order after the primary.
    #[must_use]
    pub fn with_fallbacks<I>(mut self, fallbacks: I) -> Self
    where
        I: IntoIterator,
        I::Item: Into<ProviderId>,
    {
        self.fallbacks.extend(fallbacks.into_iter().map(Into::into));
        self
    }

    fn ids(&self) -> impl Iterator<Item = &ProviderId> {
        std::iter::once(&self.primary).chain(&self.fallbacks)
    }
}

/// A router could not be built. Every variant is a configuration bug.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RouteError {
    /// Two providers share an id.
    #[error("provider `{0}` is registered twice")]
    DuplicateProvider(ProviderId),
    /// The route names a provider that is not registered.
    #[error("route references unknown provider `{0}`")]
    UnknownProvider(ProviderId),
    /// The route names the same provider more than once.
    #[error("provider `{0}` appears more than once in the route")]
    RepeatedInRoute(ProviderId),
    /// A provider was registered for the wrong layer.
    #[error("provider `{provider}` serves the {actual} layer, not {expected}")]
    WrongLayer {
        /// The provider.
        provider: ProviderId,
        /// The router's layer.
        expected: Layer,
        /// The provider's layer.
        actual: Layer,
    },
}

/// Why a provider was left out of a request's candidates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExclusionReason {
    /// Policy forbids sending this data to it.
    Egress(EgressDenied),
    /// It does not support the request's language.
    Language(LanguageTag),
}

/// A provider left out of a request's candidates, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Exclusion {
    /// The provider.
    pub provider: ProviderId,
    /// Why it was left out.
    pub reason: ExclusionReason,
}

/// Providers eligible for one request, in route order, plus the ones excluded.
pub struct Candidates<P: ?Sized> {
    /// Eligible providers, to try in order.
    pub allowed: Vec<Arc<P>>,
    /// Providers left out, with reasons.
    pub excluded: Vec<Exclusion>,
}

/// Selects providers of type `P` (usually `dyn SomePort`) for one layer.
pub struct Router<P: ?Sized> {
    layer: Layer,
    ordered: Vec<Arc<P>>,
    egress: Arc<EgressPolicy>,
}

impl<P: ?Sized + Describe> Router<P> {
    /// Builds a router for `layer` that tries `route` over `providers`.
    ///
    /// Providers not named in the route may be registered; they are ignored.
    pub fn new(
        layer: Layer,
        route: &Route,
        providers: impl IntoIterator<Item = Arc<P>>,
        egress: Arc<EgressPolicy>,
    ) -> Result<Self, RouteError> {
        let mut by_id: HashMap<ProviderId, Arc<P>> = HashMap::new();
        for provider in providers {
            let descriptor = provider.descriptor();
            if descriptor.layer != layer {
                return Err(RouteError::WrongLayer {
                    provider: descriptor.id.clone(),
                    expected: layer,
                    actual: descriptor.layer,
                });
            }
            let id = descriptor.id.clone();
            if by_id.insert(id.clone(), provider).is_some() {
                return Err(RouteError::DuplicateProvider(id));
            }
        }

        let mut seen = HashSet::new();
        let mut ordered = Vec::new();
        for id in route.ids() {
            if !seen.insert(id) {
                return Err(RouteError::RepeatedInRoute(id.clone()));
            }
            let provider = by_id
                .get(id)
                .ok_or_else(|| RouteError::UnknownProvider(id.clone()))?;
            ordered.push(Arc::clone(provider));
        }

        Ok(Self {
            layer,
            ordered,
            egress,
        })
    }

    /// The layer this router serves.
    #[must_use]
    pub fn layer(&self) -> Layer {
        self.layer
    }

    /// Providers that may serve a request carrying `label` data in `language`.
    #[must_use]
    pub fn candidates(&self, label: Sensitivity, language: Option<&LanguageTag>) -> Candidates<P> {
        let mut allowed = Vec::new();
        let mut excluded = Vec::new();
        for provider in &self.ordered {
            let descriptor = provider.descriptor();
            if let Some(language) = language.filter(|l| !descriptor.supports_language(l)) {
                excluded.push(Exclusion {
                    provider: descriptor.id.clone(),
                    reason: ExclusionReason::Language(language.clone()),
                });
                continue;
            }
            match self.egress.check(label, descriptor) {
                Ok(()) => allowed.push(Arc::clone(provider)),
                Err(denied) => excluded.push(Exclusion {
                    provider: descriptor.id.clone(),
                    reason: ExclusionReason::Egress(denied),
                }),
            }
        }
        Candidates { allowed, excluded }
    }
}

#[cfg(test)]
mod tests {
    use tsetsen_core::{Locality, ProviderDescriptor};

    use super::*;

    struct Fake(ProviderDescriptor);

    impl Describe for Fake {
        fn descriptor(&self) -> &ProviderDescriptor {
            &self.0
        }
    }

    fn local(id: &str) -> Arc<Fake> {
        Arc::new(Fake(ProviderDescriptor::new(
            id,
            Layer::Stt,
            Locality::Local,
        )))
    }

    fn remote(id: &str) -> Arc<Fake> {
        Arc::new(Fake(ProviderDescriptor::new(
            id,
            Layer::Stt,
            Locality::Remote {
                host: "api.example.com".into(),
            },
        )))
    }

    fn ids(candidates: &Candidates<Fake>) -> Vec<&str> {
        candidates
            .allowed
            .iter()
            .map(|p| p.descriptor().id.as_str())
            .collect()
    }

    fn router(route: &Route, providers: Vec<Arc<Fake>>) -> Result<Router<Fake>, RouteError> {
        Router::new(Layer::Stt, route, providers, Arc::new(EgressPolicy::new()))
    }

    #[test]
    fn keeps_route_order() {
        let route = Route::new("b").with_fallbacks(["a"]);
        let router = router(&route, vec![local("a"), local("b")]).unwrap();
        assert_eq!(
            ids(&router.candidates(Sensitivity::Audio, None)),
            ["b", "a"]
        );
    }

    #[test]
    fn excludes_remote_providers_without_a_grant() {
        let route = Route::new("cloud").with_fallbacks(["local"]);
        let router = router(&route, vec![remote("cloud"), local("local")]).unwrap();
        let candidates = router.candidates(Sensitivity::Audio, None);
        assert_eq!(ids(&candidates), ["local"]);
        assert!(matches!(
            candidates.excluded.as_slice(),
            [Exclusion {
                reason: ExclusionReason::Egress(_),
                ..
            }]
        ));
    }

    #[test]
    fn falls_through_to_a_provider_that_speaks_the_language() {
        let english_only = Arc::new(Fake(
            ProviderDescriptor::new("parakeet", Layer::Stt, Locality::Local)
                .with_languages([LanguageTag::from("en")]),
        ));
        let route = Route::new("parakeet").with_fallbacks(["whisper"]);
        let router = router(&route, vec![english_only, local("whisper")]).unwrap();

        let mongolian = LanguageTag::from("mn");
        assert_eq!(
            ids(&router.candidates(Sensitivity::Audio, Some(&mongolian))),
            ["whisper"]
        );
        let english = LanguageTag::from("en-US");
        assert_eq!(
            ids(&router.candidates(Sensitivity::Audio, Some(&english))),
            ["parakeet", "whisper"]
        );
    }

    #[test]
    fn rejects_configuration_bugs() {
        assert_eq!(
            router(&Route::new("missing"), vec![local("a")]).err(),
            Some(RouteError::UnknownProvider("missing".into()))
        );
        assert_eq!(
            router(&Route::new("a"), vec![local("a"), local("a")]).err(),
            Some(RouteError::DuplicateProvider("a".into()))
        );
        assert_eq!(
            router(&Route::new("a").with_fallbacks(["a"]), vec![local("a")]).err(),
            Some(RouteError::RepeatedInRoute("a".into()))
        );
        let wrong = Arc::new(Fake(ProviderDescriptor::new(
            "llm",
            Layer::Rewrite,
            Locality::Local,
        )));
        assert!(matches!(
            router(&Route::new("llm"), vec![wrong]).err(),
            Some(RouteError::WrongLayer { .. })
        ));
    }
}
