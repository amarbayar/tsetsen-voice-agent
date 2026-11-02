//! Invariants that must hold for every configuration, not just the examples we
//! thought of.

use proptest::prelude::*;
use tsetsen_core::{Layer, Locality, ProviderDescriptor, ProviderId, Sensitivity};
use tsetsen_policy::{EgressPolicy, GateDecision, RiskGate, RiskThresholds};

fn sensitivity() -> impl Strategy<Value = Sensitivity> {
    prop::sample::select(Sensitivity::ALL.to_vec())
}

fn remote(id: &str) -> ProviderDescriptor {
    ProviderDescriptor::new(
        id,
        Layer::Rewrite,
        Locality::Remote {
            host: "api.example.com".into(),
        },
    )
}

/// Applies arbitrary grants to providers p0..p3; grants above the ceiling are
/// refused, which is itself part of what is being tested.
fn policy_with(grants: &[(u8, Sensitivity)]) -> EgressPolicy {
    let mut policy = EgressPolicy::new();
    for (provider, label) in grants {
        let _ = policy.grant_remote(ProviderId::new(format!("p{}", provider % 4)), *label);
    }
    policy
}

proptest! {
    #[test]
    fn secret_data_never_reaches_a_remote_provider(
        grants in prop::collection::vec((any::<u8>(), sensitivity()), 0..16),
        provider in 0u8..4,
    ) {
        let policy = policy_with(&grants);
        let descriptor = remote(&format!("p{provider}"));
        prop_assert!(policy.check(Sensitivity::Secret, &descriptor).is_err());
    }

    #[test]
    fn local_providers_accept_every_label(label in sensitivity()) {
        let local = ProviderDescriptor::new("local", Layer::Stt, Locality::Local);
        prop_assert!(EgressPolicy::new().check(label, &local).is_ok());
    }

    #[test]
    fn remote_providers_accept_nothing_by_default(label in sensitivity()) {
        prop_assert!(EgressPolicy::new().check(label, &remote("p0")).is_err());
    }

    #[test]
    fn allowing_a_label_allows_every_less_sensitive_label(
        grants in prop::collection::vec((any::<u8>(), sensitivity()), 0..16),
        provider in 0u8..4,
        label in sensitivity(),
    ) {
        let policy = policy_with(&grants);
        let descriptor = remote(&format!("p{provider}"));
        if policy.check(label, &descriptor).is_ok() {
            for lower in Sensitivity::ALL.into_iter().filter(|l| *l <= label) {
                prop_assert!(policy.check(lower, &descriptor).is_ok());
            }
        }
    }

    #[test]
    fn nothing_acts_below_the_floor(confidence in 0.0f32..0.55, destructive in 0.0f32..=1.0) {
        prop_assert_eq!(RiskGate::default().evaluate(confidence, destructive), GateDecision::Abstain);
    }

    #[test]
    fn destructive_actions_need_the_higher_bar(confidence in 0.0f32..0.90, destructive in 0.5f32..=1.0) {
        prop_assert_ne!(RiskGate::default().evaluate(confidence, destructive), GateDecision::Act);
    }

    #[test]
    fn more_confidence_never_makes_the_gate_stricter(
        low in 0.0f32..=1.0,
        delta in 0.0f32..=1.0,
        destructive in 0.0f32..=1.0,
    ) {
        let gate = RiskGate::default();
        let high = (low + delta).min(1.0);
        let rank = |d: GateDecision| match d {
            GateDecision::Abstain => 0,
            GateDecision::Confirm => 1,
            GateDecision::Act => 2,
        };
        prop_assert!(rank(gate.evaluate(high, destructive)) >= rank(gate.evaluate(low, destructive)));
    }
}

#[test]
fn grants_above_the_ceiling_are_refused() {
    let mut policy = EgressPolicy::new();
    assert!(
        policy
            .grant_remote("p0".into(), Sensitivity::Secret)
            .is_err()
    );
    assert_eq!(policy.remote_grant(&"p0".into()), None);
}

#[test]
fn non_finite_inputs_abstain() {
    let gate = RiskGate::default();
    assert_eq!(gate.evaluate(f32::NAN, 0.0), GateDecision::Abstain);
    assert_eq!(gate.evaluate(1.0, f32::INFINITY), GateDecision::Abstain);
}

#[test]
fn misordered_thresholds_are_rejected() {
    let thresholds = RiskThresholds {
        floor: 0.8,
        act: 0.7,
        ..RiskThresholds::default()
    };
    assert!(RiskGate::new(thresholds).is_err());
    assert!(
        RiskGate::new(RiskThresholds {
            act: 1.5,
            ..RiskThresholds::default()
        })
        .is_err()
    );
}
