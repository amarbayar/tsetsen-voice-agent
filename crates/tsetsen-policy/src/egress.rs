use std::collections::HashMap;

use tsetsen_core::{Locality, ProviderDescriptor, ProviderId, Sensitivity};

use crate::PolicyError;

/// Decides which data may be sent to which provider.
///
/// - Local providers may receive anything, because the data never leaves the device.
/// - Remote providers receive nothing until the user grants them a label.
/// - No remote provider can ever receive [`Sensitivity::Secret`]: grants above
///   [`EgressPolicy::REMOTE_CEILING`] are refused when they are configured.
#[derive(Clone, Debug, Default)]
pub struct EgressPolicy {
    remote_grants: HashMap<ProviderId, Sensitivity>,
}

/// Data with `label` may not be sent to `provider`.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{label:?} data may not be sent to remote provider `{provider}`")]
pub struct EgressDenied {
    /// The provider that was refused.
    pub provider: ProviderId,
    /// The label of the data.
    pub label: Sensitivity,
    /// What the provider is granted, if anything.
    pub granted: Option<Sensitivity>,
}

impl EgressPolicy {
    /// The most sensitive label a remote provider may ever be granted.
    pub const REMOTE_CEILING: Sensitivity = Sensitivity::Audio;

    /// A policy with no remote grants: everything stays on the device.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Lets remote `provider` receive data up to and including `up_to`.
    /// Replaces any earlier grant for that provider.
    pub fn grant_remote(
        &mut self,
        provider: ProviderId,
        up_to: Sensitivity,
    ) -> Result<(), PolicyError> {
        if up_to > Self::REMOTE_CEILING {
            return Err(PolicyError::GrantAboveCeiling {
                provider,
                requested: up_to,
                ceiling: Self::REMOTE_CEILING,
            });
        }
        self.remote_grants.insert(provider, up_to);
        Ok(())
    }

    /// The label `provider` is granted, if any.
    #[must_use]
    pub fn remote_grant(&self, provider: &ProviderId) -> Option<Sensitivity> {
        self.remote_grants.get(provider).copied()
    }

    /// Allows the request only if data labeled `label` may go to `provider`.
    pub fn check(
        &self,
        label: Sensitivity,
        provider: &ProviderDescriptor,
    ) -> Result<(), EgressDenied> {
        match provider.locality {
            Locality::Local => Ok(()),
            Locality::Remote { .. } => {
                let granted = self.remote_grant(&provider.id);
                if granted.is_some_and(|granted| label <= granted) {
                    Ok(())
                } else {
                    Err(EgressDenied {
                        provider: provider.id.clone(),
                        label,
                        granted,
                    })
                }
            }
        }
    }
}
