//! Turn detection: when the user starts speaking and when they are done.
//!
//! Driven by per-frame speech probabilities from a voice activity detector,
//! with hysteresis (a higher bar to start than to continue) so a word's
//! trailing consonant does not end the turn.

use std::collections::VecDeque;
use std::time::Duration;

/// Thresholds and timings for turn detection.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EndpointConfig {
    /// Probability at or above which a frame counts as speech when idle.
    pub start_threshold: f32,
    /// Probability below which a frame counts as silence once speaking.
    pub end_threshold: f32,
    /// Speech needed before a turn has started; shorter blips are ignored.
    pub min_speech: Duration,
    /// Silence that ends the turn.
    pub end_silence: Duration,
    /// Audio kept from before speech was detected, so first syllables are
    /// not clipped.
    pub pre_roll: Duration,
    /// A turn this long ends even without silence.
    pub max_utterance: Duration,
}

impl EndpointConfig {
    /// For listening to the user's turn.
    #[must_use]
    pub fn listening() -> Self {
        Self {
            start_threshold: 0.5,
            end_threshold: 0.35,
            min_speech: Duration::from_millis(200),
            end_silence: Duration::from_millis(650),
            pre_roll: Duration::from_millis(320),
            max_utterance: Duration::from_secs(30),
        }
    }

    /// For detecting the user talking over the assistant: a higher bar, so
    /// echo the canceller missed or a cough does not stop the reply.
    #[must_use]
    pub fn barge_in() -> Self {
        Self {
            start_threshold: 0.7,
            min_speech: Duration::from_millis(350),
            ..Self::listening()
        }
    }
}

/// What a frame changed.
#[derive(Clone, Debug, PartialEq)]
pub enum TurnEvent {
    /// The user has been speaking for at least `min_speech`.
    SpeechStarted,
    /// The user finished a turn. Samples include the pre-roll.
    Utterance(Vec<f32>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Idle,
    /// Speech detected but not yet long enough to count.
    Pending {
        speech: usize,
        silence: usize,
    },
    Speaking {
        silence: usize,
    },
}

/// Turn detector over a stream of fixed-size frames.
#[derive(Debug)]
pub struct Endpointer {
    config: EndpointConfig,
    frame: Duration,
    state: State,
    pre_roll: VecDeque<f32>,
    pre_roll_len: usize,
    utterance: Vec<f32>,
}

impl Endpointer {
    /// A detector for frames of `frame_len` samples at `sample_rate_hz`.
    #[must_use]
    pub fn new(config: EndpointConfig, sample_rate_hz: u32, frame_len: usize) -> Self {
        #[allow(clippy::cast_precision_loss)]
        let frame = Duration::from_secs_f64(frame_len as f64 / f64::from(sample_rate_hz.max(1)));
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let pre_roll_len = (config.pre_roll.as_secs_f64() * f64::from(sample_rate_hz)) as usize;
        Self {
            config,
            frame,
            state: State::Idle,
            pre_roll: VecDeque::with_capacity(pre_roll_len + frame_len),
            pre_roll_len,
            utterance: Vec::new(),
        }
    }

    /// Switches thresholds without losing a turn in progress.
    pub fn set_config(&mut self, config: EndpointConfig) {
        self.config = config;
    }

    /// Whether the user is mid-turn.
    #[must_use]
    pub fn in_speech(&self) -> bool {
        self.state != State::Idle
    }

    /// Discards any turn in progress.
    pub fn reset(&mut self) {
        self.state = State::Idle;
        self.utterance.clear();
        self.pre_roll.clear();
    }

    fn frames(&self, duration: Duration) -> usize {
        // Round up: a 650 ms requirement with 32 ms frames needs 21 frames.
        duration
            .as_nanos()
            .div_ceil(self.frame.as_nanos().max(1))
            .try_into()
            .unwrap_or(usize::MAX)
    }

    /// Feeds one frame and its speech probability.
    pub fn push(&mut self, frame: &[f32], probability: f32) -> Option<TurnEvent> {
        let speech = probability >= self.config.start_threshold;
        let silent = probability < self.config.end_threshold;
        match self.state {
            State::Idle => {
                if speech {
                    self.utterance.clear();
                    self.utterance.extend(self.pre_roll.drain(..));
                    self.utterance.extend_from_slice(frame);
                    self.state = State::Pending {
                        speech: 1,
                        silence: 0,
                    };
                    return self.check_started();
                }
                self.pre_roll.extend(frame);
                let excess = self.pre_roll.len().saturating_sub(self.pre_roll_len);
                self.pre_roll.drain(..excess);
                None
            }
            State::Pending {
                speech: count,
                silence,
            } => {
                self.utterance.extend_from_slice(frame);
                let (count, silence) = if speech {
                    (count + 1, 0)
                } else if silent {
                    (count, silence + 1)
                } else {
                    (count, silence)
                };
                if silence >= self.frames(self.config.end_silence) {
                    // A blip, not a turn.
                    self.reset();
                    return None;
                }
                self.state = State::Pending {
                    speech: count,
                    silence,
                };
                self.check_started()
            }
            State::Speaking { silence } => {
                self.utterance.extend_from_slice(frame);
                let silence = if silent { silence + 1 } else { 0 };
                #[allow(clippy::cast_precision_loss)]
                let too_long = self.utterance.len() as f64 * self.frame.as_secs_f64()
                    / frame.len().max(1) as f64
                    >= self.config.max_utterance.as_secs_f64();
                if silence >= self.frames(self.config.end_silence) || too_long {
                    self.state = State::Idle;
                    self.pre_roll.clear();
                    return Some(TurnEvent::Utterance(std::mem::take(&mut self.utterance)));
                }
                self.state = State::Speaking { silence };
                None
            }
        }
    }

    fn check_started(&mut self) -> Option<TurnEvent> {
        if let State::Pending { speech, .. } = self.state
            && speech >= self.frames(self.config.min_speech)
        {
            self.state = State::Speaking { silence: 0 };
            return Some(TurnEvent::SpeechStarted);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: usize = 512; // 32 ms at 16 kHz

    fn endpointer() -> Endpointer {
        Endpointer::new(EndpointConfig::listening(), 16_000, FRAME)
    }

    /// Feeds `(probability, frames)` runs; returns the events with the frame
    /// index they happened at.
    fn feed(endpointer: &mut Endpointer, runs: &[(f32, usize)]) -> Vec<(usize, TurnEvent)> {
        let mut events = Vec::new();
        let mut index = 0;
        for &(probability, frames) in runs {
            for _ in 0..frames {
                #[allow(clippy::cast_precision_loss)]
                let frame = vec![index as f32; FRAME];
                if let Some(event) = endpointer.push(&frame, probability) {
                    events.push((index, event));
                }
                index += 1;
            }
        }
        events
    }

    #[test]
    fn detects_a_turn_with_pre_roll_and_trailing_silence() {
        let mut e = endpointer();
        let events = feed(&mut e, &[(0.0, 20), (0.9, 30), (0.0, 25)]);
        assert_eq!(events.len(), 2);
        // 200 ms of speech is 7 frames: started on the 7th speech frame.
        assert_eq!(events[0], (26, TurnEvent::SpeechStarted));
        // 650 ms of silence is 21 frames.
        let (at, TurnEvent::Utterance(samples)) = &events[1] else {
            panic!()
        };
        assert_eq!(*at, 70);
        // 10 pre-roll frames (320 ms), 30 speech, 21 silence.
        assert_eq!(samples.len(), (10 + 30 + 21) * FRAME);
        assert!(
            (samples[0] - 10.0).abs() < f32::EPSILON,
            "pre-roll starts 10 frames early"
        );
    }

    #[test]
    fn ignores_blips_shorter_than_min_speech() {
        let mut e = endpointer();
        assert!(feed(&mut e, &[(0.9, 3), (0.0, 40)]).is_empty());
        assert!(!e.in_speech());
    }

    #[test]
    fn brief_pauses_do_not_end_the_turn() {
        let mut e = endpointer();
        let events = feed(&mut e, &[(0.9, 20), (0.0, 15), (0.9, 20), (0.0, 25)]);
        let utterances = events
            .iter()
            .filter(|(_, e)| matches!(e, TurnEvent::Utterance(_)))
            .count();
        assert_eq!(utterances, 1);
    }

    #[test]
    fn uncertain_frames_keep_the_turn_going() {
        let mut e = endpointer();
        // 0.4 is between the end (0.35) and start (0.5) thresholds.
        let events = feed(&mut e, &[(0.9, 10), (0.4, 50)]);
        assert_eq!(events, [(6, TurnEvent::SpeechStarted)]);
        assert!(e.in_speech());
    }

    #[test]
    fn caps_turn_length() {
        let mut e = Endpointer::new(
            EndpointConfig {
                max_utterance: Duration::from_secs(1),
                ..EndpointConfig::listening()
            },
            16_000,
            FRAME,
        );
        let events = feed(&mut e, &[(0.9, 40)]);
        // 1 s is 31.25 frames: the turn ends on frame 31, then a new one starts.
        let ended: Vec<usize> = events
            .iter()
            .filter(|(_, e)| matches!(e, TurnEvent::Utterance(_)))
            .map(|&(at, _)| at)
            .collect();
        assert_eq!(ended, [31]);
    }

    #[test]
    fn barge_in_needs_more_confident_and_longer_speech() {
        let mut e = Endpointer::new(EndpointConfig::barge_in(), 16_000, FRAME);
        assert!(
            feed(&mut e, &[(0.6, 30)]).is_empty(),
            "below the barge-in threshold"
        );
        e.reset();
        let events = feed(&mut e, &[(0.9, 15)]);
        assert_eq!(events, [(10, TurnEvent::SpeechStarted)]); // 350 ms = 11 frames
    }
}
