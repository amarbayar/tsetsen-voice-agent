use crate::PolicyError;

/// Confidence bars for acting without asking.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RiskThresholds {
    /// Below this target confidence, nothing runs.
    pub floor: f32,
    /// Target confidence needed for an ordinary action.
    pub act: f32,
    /// Target confidence needed for an action judged destructive.
    pub destructive_act: f32,
    /// Probability above which an action counts as destructive.
    pub destructive_threshold: f32,
}

impl Default for RiskThresholds {
    fn default() -> Self {
        Self {
            floor: 0.55,
            act: 0.70,
            destructive_act: 0.90,
            destructive_threshold: 0.50,
        }
    }
}

/// What to do with a proposed action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GateDecision {
    /// Confident enough for the action's risk: run it.
    Act,
    /// Plausible but under the bar: ask the user first.
    Confirm,
    /// Too uncertain to even propose.
    Abstain,
}

/// Turns target confidence and destructiveness into a decision.
///
/// The model's answer says *what* to do; this gate decides *whether* to do it
/// without asking.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RiskGate {
    thresholds: RiskThresholds,
}

impl RiskGate {
    /// Validates `thresholds`: all within `0.0..=1.0`, and
    /// `floor <= act <= destructive_act`.
    pub fn new(thresholds: RiskThresholds) -> Result<Self, PolicyError> {
        let RiskThresholds {
            floor,
            act,
            destructive_act,
            destructive_threshold,
        } = thresholds;
        if ![floor, act, destructive_act, destructive_threshold]
            .iter()
            .all(|value| (0.0..=1.0).contains(value))
        {
            return Err(PolicyError::InvalidThresholds(
                "every threshold must be within 0.0..=1.0",
            ));
        }
        if !(floor <= act && act <= destructive_act) {
            return Err(PolicyError::InvalidThresholds(
                "expected floor <= act <= destructive_act",
            ));
        }
        Ok(Self { thresholds })
    }

    /// The configured thresholds.
    #[must_use]
    pub fn thresholds(&self) -> RiskThresholds {
        self.thresholds
    }

    /// Decides whether an action targeting an element chosen with
    /// `target_confidence`, and judged destructive with
    /// `destructive_probability`, may run. Non-finite inputs abstain.
    #[must_use]
    pub fn evaluate(&self, target_confidence: f32, destructive_probability: f32) -> GateDecision {
        let t = &self.thresholds;
        if !target_confidence.is_finite()
            || !destructive_probability.is_finite()
            || target_confidence < t.floor
        {
            return GateDecision::Abstain;
        }
        let bar = if destructive_probability >= t.destructive_threshold {
            t.destructive_act
        } else {
            t.act
        };
        if target_confidence >= bar {
            GateDecision::Act
        } else {
            GateDecision::Confirm
        }
    }
}
