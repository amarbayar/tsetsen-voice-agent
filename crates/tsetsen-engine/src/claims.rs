//! Catching a model that talks about acting instead of acting.
//!
//! Small models sometimes end a reply with "I'll send it now" and no tool
//! call, or "I've sent it" when nothing ran. For someone who cannot check, a
//! false "it's sent" is the worst possible failure. These checks run on the
//! model's own words after a step, and give it one chance to do what it said
//! or to correct itself.

/// What a final reply says about actions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Claim {
    /// Nothing about acting.
    None,
    /// It says it will do something now: "I'll send it".
    Announces,
    /// It says something was done: "I've sent it".
    Completed,
}

const ANNOUNCING: &[&str] = &[
    "i'll ",
    "i will ",
    "let me ",
    "i'm going to ",
    "i am going to ",
    "i'm now ",
    "i am now ",
    "now i'll ",
];

/// "Let me know" and the like are not announcements.
const NOT_ANNOUNCING: &[&str] = &["let me know", "i'll be here", "i will be here", "i'll wait"];

const COMPLETED: &[&str] = &[
    "i've sent",
    "i have sent",
    "i sent",
    "has been sent",
    "have been sent",
    "was sent",
    "were sent",
    "i've emailed",
    "i emailed",
    "i've saved",
    "i have saved",
    "i saved",
    "i've remembered",
    "i've deleted",
    "i deleted",
    "i've opened",
    "i opened",
    "it's on its way",
    "is on its way",
    "are on their way",
    "all done",
    "i've done",
    "i have done",
];

/// Reads `text` for claims about acting. A reply that ends with a question
/// is asking, not announcing.
pub(crate) fn claim(text: &str) -> Claim {
    read(text, true)
}

/// Like [`claim`], for the front desk, which hands work on rather than
/// asking about it: "I'll find it. Was it on your phone?" still promises
/// work that nobody was given.
pub(crate) fn front_claim(text: &str) -> Claim {
    read(text, false)
}

fn read(text: &str, questions_excuse: bool) -> Claim {
    let lowered = text.to_lowercase().replace('’', "'");
    if COMPLETED.iter().any(|c| lowered.contains(c)) {
        return Claim::Completed;
    }
    if questions_excuse && lowered.trim_end().ends_with('?') {
        return Claim::None;
    }
    let announcing = ANNOUNCING.iter().any(|a| {
        lowered
            .match_indices(a)
            .any(|(at, _)| !NOT_ANNOUNCING.iter().any(|n| lowered[at..].starts_with(n)))
    });
    if announcing {
        Claim::Announces
    } else {
        Claim::None
    }
}

/// What the model is told when its words and its actions disagree.
pub(crate) fn nudge(claim: Claim) -> Option<&'static str> {
    match claim {
        Claim::None => None,
        Claim::Announces => Some(
            "(Note from the system, not the user: you said you would do something but called no \
             tool, so nothing has happened. Call the tool now. If you need something from the \
             user first, ask them in one short sentence.)",
        ),
        Claim::Completed => Some(
            "(Note from the system, not the user: no tool ran in this reply, so nothing has been \
             done. Do it now with the right tool, or tell the user plainly, in one sentence, \
             that it has not been done yet.)",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_claims() {
        for text in [
            "Great! I'll save David as your son and send the five pictures to him.",
            "Okay, let me send that now.",
            "I'm going to email him.",
        ] {
            assert_eq!(claim(text), Claim::Announces, "{text}");
        }
        for text in [
            "I've saved David Smith as your son and sent the five pictures. He should receive them shortly!",
            "It's sent. David should have it in a minute.",
            "The email has been sent.",
            "All done!",
        ] {
            // "It's sent" alone is not listed: checked against the actions.
            if text.starts_with("It's sent") {
                continue;
            }
            assert_eq!(claim(text), Claim::Completed, "{text}");
        }
        for text in [
            "Octopuses have three hearts.",
            "I'll send them to David Smith. Is that right?",
            "Let me know if you need anything else.",
            "Which pictures would you like to send?",
        ] {
            assert_eq!(claim(text), Claim::None, "{text}");
        }
        assert_eq!(
            front_claim("Let me find it for you. Did you take it with your phone?"),
            Claim::Announces
        );
        assert_eq!(front_claim("Which pictures would you like?"), Claim::None);
    }
}
