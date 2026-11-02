//! Policy for Tsetsen: which data may go to which provider, and which actions
//! may run without asking.
//!
//! Everything here is pure, synchronous logic, so its invariants can be
//! property-tested exhaustively.

mod action;
mod egress;
mod risk;

pub use action::{
    Approval, ConfirmPolicy, Consequence, approval, click_consequence, key_consequence,
    typing_consequence,
};
pub use egress::{EgressDenied, EgressPolicy};
pub use risk::{GateDecision, RiskGate, RiskThresholds};

use tsetsen_core::{ProviderId, Sensitivity};

/// A policy could not be configured as requested.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PolicyError {
    /// The grant would let a remote provider receive data above
    /// [`EgressPolicy::REMOTE_CEILING`].
    #[error(
        "remote provider `{provider}` cannot be granted {requested:?} data; the ceiling is {ceiling:?}"
    )]
    GrantAboveCeiling {
        /// The provider.
        provider: ProviderId,
        /// The label that was requested.
        requested: Sensitivity,
        /// The highest label any remote provider may receive.
        ceiling: Sensitivity,
    },
    /// Risk thresholds were out of range or out of order.
    #[error("invalid risk thresholds: {0}")]
    InvalidThresholds(&'static str),
}
