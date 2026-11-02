use async_trait::async_trait;

use crate::{Ctx, Describe, ProviderError, QuestionKey};

/// One typed question for a decision model. Answers are always distributions
/// over fixed outcomes, never free text.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Question {
    /// Outcomes [`Question::YES`] and [`Question::NO`].
    YesNo {
        /// Identifies the answer.
        key: QuestionKey,
        /// What "yes" means.
        criterion: String,
    },
    /// One of `options`, plus [`Question::NONE`] when `allow_none` is set.
    Choice {
        /// Identifies the answer.
        key: QuestionKey,
        /// The candidates.
        options: Vec<ChoiceOption>,
        /// Whether "none of these" is an acceptable answer.
        allow_none: bool,
    },
    /// An ordinal score with outcomes `"1"` to `levels`.
    Score {
        /// Identifies the answer.
        key: QuestionKey,
        /// Number of levels, 2 to 10.
        levels: u8,
        /// What the levels mean.
        rubric: String,
    },
}

impl Question {
    /// Outcome name for a positive yes/no answer.
    pub const YES: &str = "yes";
    /// Outcome name for a negative yes/no answer.
    pub const NO: &str = "no";
    /// Outcome name for "none of the options".
    pub const NONE: &str = "none";

    /// The key the answer will carry.
    #[must_use]
    pub fn key(&self) -> &QuestionKey {
        match self {
            Self::YesNo { key, .. } | Self::Choice { key, .. } | Self::Score { key, .. } => key,
        }
    }
}

/// One candidate in a [`Question::Choice`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChoiceOption {
    /// The outcome name returned in the answer.
    pub id: String,
    /// What the option is, in words the model can judge.
    pub description: String,
}

/// What the decision is about.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DecisionState {
    /// A textual description of the current state.
    pub text: String,
}

/// A probability distribution over outcomes, normalized to sum to 1.
#[derive(Clone, Debug, PartialEq)]
pub struct Distribution(Vec<(String, f32)>);

/// A [`Distribution`] could not be built.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum InvalidDistribution {
    /// No outcomes were given.
    #[error("distribution has no outcomes")]
    Empty,
    /// A probability was negative, NaN or infinite.
    #[error("outcome `{0}` has an invalid probability")]
    InvalidProbability(String),
    /// All probabilities were zero.
    #[error("distribution has zero total probability")]
    ZeroMass,
}

impl Distribution {
    /// Normalizes `entries` and orders them from most to least likely.
    pub fn new(mut entries: Vec<(String, f32)>) -> Result<Self, InvalidDistribution> {
        if entries.is_empty() {
            return Err(InvalidDistribution::Empty);
        }
        if let Some((outcome, _)) = entries.iter().find(|(_, p)| !p.is_finite() || *p < 0.0) {
            return Err(InvalidDistribution::InvalidProbability(outcome.clone()));
        }
        let total: f32 = entries.iter().map(|(_, p)| p).sum();
        if total <= 0.0 {
            return Err(InvalidDistribution::ZeroMass);
        }
        for (_, p) in &mut entries {
            *p /= total;
        }
        entries.sort_by(|a, b| b.1.total_cmp(&a.1));
        Ok(Self(entries))
    }

    /// The most likely outcome and its probability.
    #[must_use]
    pub fn top(&self) -> (&str, f32) {
        let (outcome, p) = &self.0[0]; // Non-empty by construction.
        (outcome, *p)
    }

    /// Probability of `outcome`; zero if it is not in the distribution.
    #[must_use]
    pub fn probability(&self, outcome: &str) -> f32 {
        self.0
            .iter()
            .find(|(name, _)| name == outcome)
            .map_or(0.0, |(_, p)| *p)
    }

    /// Outcomes from most to least likely.
    pub fn iter(&self) -> impl Iterator<Item = (&str, f32)> {
        self.0.iter().map(|(outcome, p)| (outcome.as_str(), *p))
    }
}

/// The answer to one [`Question`].
#[derive(Clone, Debug, PartialEq)]
pub struct Answer {
    /// The question's key.
    pub key: QuestionKey,
    /// Probabilities over the question's outcomes.
    pub distribution: Distribution,
}

/// A decision model: answers typed questions with calibrated probabilities
/// (for example d1-2B or Jev).
#[async_trait]
pub trait Decider: Describe + Send + Sync {
    /// Answers every question about `state`, in any order.
    async fn decide(
        &self,
        state: &DecisionState,
        questions: &[Question],
        cx: &Ctx,
    ) -> Result<Vec<Answer>, ProviderError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dist(entries: &[(&str, f32)]) -> Result<Distribution, InvalidDistribution> {
        Distribution::new(entries.iter().map(|(o, p)| ((*o).to_owned(), *p)).collect())
    }

    #[test]
    fn normalizes_and_orders_outcomes() {
        let d = dist(&[("no", 1.0), ("yes", 3.0)]).unwrap();
        assert_eq!(d.top(), ("yes", 0.75));
        assert!((d.probability("no") - 0.25).abs() < f32::EPSILON);
        assert!(d.probability("maybe").abs() < f32::EPSILON);
    }

    #[test]
    fn rejects_invalid_input() {
        assert_eq!(dist(&[]), Err(InvalidDistribution::Empty));
        assert_eq!(
            dist(&[("a", f32::NAN)]),
            Err(InvalidDistribution::InvalidProbability("a".into()))
        );
        assert_eq!(
            dist(&[("a", -0.1), ("b", 1.0)]),
            Err(InvalidDistribution::InvalidProbability("a".into()))
        );
        assert_eq!(dist(&[("a", 0.0)]), Err(InvalidDistribution::ZeroMass));
    }
}
