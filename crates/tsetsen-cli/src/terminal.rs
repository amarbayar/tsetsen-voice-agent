//! Shows the conversation in the terminal as it happens.

use std::io::Write;
use std::sync::Mutex;

use tsetsen_engine::{ActionOutcome, ConversationEvent, Observer, TurnOutcome, TurnResult};

/// Prints each side of the conversation.
#[derive(Default)]
pub(crate) struct Terminal {
    speaking: Mutex<bool>,
}

impl Terminal {
    fn end_line(&self) {
        let mut speaking = self
            .speaking
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *speaking {
            println!();
            *speaking = false;
        }
    }

    /// After each turn: latency, and anything that went wrong.
    pub(crate) fn turn(&self, result: &TurnResult) {
        self.end_line();
        match result {
            Ok(TurnOutcome::Replied(reply) | TurnOutcome::Announced(reply)) => {
                let ms = |t: Option<std::time::Duration>| {
                    t.map_or_else(|| "–".to_owned(), |t| format!("{} ms", t.as_millis()))
                };
                println!(
                    "\x1b[2m        first word {} · heard {} ms · thinker {}{}\x1b[0m",
                    ms(reply.timing.first_audio),
                    reply.timing.transcribed.as_millis(),
                    ms(reply.timing.first_text),
                    if reply.interrupted {
                        " · interrupted"
                    } else {
                        ""
                    }
                );
            }
            Ok(TurnOutcome::NothingHeard { .. }) => {}
            Err(error) => println!("\x1b[31m  error: {error}\x1b[0m"),
        }
        let _ = std::io::stdout().flush();
    }
}

impl Observer for Terminal {
    fn on_event(&self, event: ConversationEvent<'_>) {
        match event {
            ConversationEvent::Listening => {}
            ConversationEvent::Heard(text) => {
                self.end_line();
                println!("\x1b[1myou\x1b[0m     › {text}");
            }
            ConversationEvent::Speaking(clause) => {
                let mut speaking = self
                    .speaking
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if *speaking {
                    print!(" {clause}");
                } else {
                    print!("\x1b[1;36mtsetsen\x1b[0m › {clause}");
                    *speaking = true;
                }
            }
            ConversationEvent::Action(action) => {
                self.end_line();
                let (mark, note) = match &action.outcome {
                    ActionOutcome::Done => ("\x1b[32m✓\x1b[0m", String::new()),
                    ActionOutcome::Failed(why) => ("\x1b[31m✗\x1b[0m", format!(" — {why}")),
                    ActionOutcome::AwaitingApproval => {
                        ("\x1b[33m?\x1b[0m", " — waiting for your OK".to_owned())
                    }
                    ActionOutcome::Declined => ("\x1b[2m–\x1b[0m", " — not done".to_owned()),
                    ActionOutcome::Interrupted => (
                        "\x1b[33m!\x1b[0m",
                        " — interrupted; may have happened".to_owned(),
                    ),
                };
                println!(
                    "\x1b[2m        ⚙ {}\x1b[0m {mark}\x1b[2m{note}\x1b[0m",
                    action.summary
                );
            }
            ConversationEvent::Interrupted => {
                print!(" \x1b[2m—\x1b[0m");
            }
            ConversationEvent::Problem(problem) => {
                self.end_line();
                println!("\x1b[33m  {problem}\x1b[0m");
            }
        }
        let _ = std::io::stdout().flush();
    }
}
