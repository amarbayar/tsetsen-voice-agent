//! Which desktop actions run at once, and which wait for the user's spoken OK.
//!
//! Classification is deliberately conservative: anything not known to be
//! reversible counts as a commitment. A wrong guess costs one extra question,
//! never an email sent or a file deleted by mistake. A decision model (d1,
//! Jev) can later refine this through [`crate::RiskGate`]; these rules stay as
//! the floor.

use crate::GateDecision;

/// What an action can do to the user's world.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Consequence {
    /// Only reads.
    Observe,
    /// Changes something the user can trivially undo: opening an app,
    /// switching tabs, typing into a field without submitting it.
    Reversible,
    /// May send, submit, delete, buy or otherwise commit.
    Commit,
}

/// When to ask before acting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ConfirmPolicy {
    /// Ask before commitments only.
    #[default]
    Commits,
    /// Ask before anything that changes state.
    AllChanges,
}

impl ConfirmPolicy {
    /// Whether an action with `consequence` may run without asking.
    #[must_use]
    pub fn decide(self, consequence: Consequence) -> GateDecision {
        match (self, consequence) {
            (_, Consequence::Observe) | (Self::Commits, Consequence::Reversible) => {
                GateDecision::Act
            }
            _ => GateDecision::Confirm,
        }
    }
}

/// The primary shortcut modifier: `cmd` on macOS, `ctrl` elsewhere.
const PRIMARY: &str = if cfg!(target_os = "macos") {
    "cmd"
} else {
    "ctrl"
};

/// Shortcuts that only navigate, select, copy or undo. Written with
/// `primary` for the platform's main modifier.
const REVERSIBLE_KEYS: &[&str] = &[
    "tab",
    "shift+tab",
    "escape",
    "up",
    "down",
    "left",
    "right",
    "home",
    "end",
    "pageup",
    "pagedown",
    "shift+up",
    "shift+down",
    "shift+left",
    "shift+right",
    "alt+left",
    "alt+right",
    "primary+up",
    "primary+down",
    "primary+left",
    "primary+right",
    "primary+a",
    "primary+c",
    "primary+z",
    "primary+shift+z",
    "primary+f",
    "primary+g",
    "primary+t",
    "primary+n",
    "primary+l",
    "primary+[",
    "primary+]",
    "primary+=",
    "primary+-",
    "primary+0",
    "primary+1",
    "primary+2",
    "primary+3",
    "primary+4",
    "primary+5",
    "primary+6",
    "primary+7",
    "primary+8",
    "primary+9",
    "primary+,",
    "ctrl+tab",
    "ctrl+shift+tab",
];

/// Canonical form: lowercase, modifiers in a fixed order, aliases folded.
fn normalize_combo(combo: &str) -> Option<String> {
    const MODIFIERS: [&str; 5] = ["primary", "ctrl", "alt", "shift", "fn"];
    let mut modifiers = [false; MODIFIERS.len()];
    let mut key = None;
    for part in combo.split('+').map(str::trim).filter(|p| !p.is_empty()) {
        let part = part.to_lowercase();
        let modifier = match part.as_str() {
            "cmd" | "command" | "meta" | "super" | "win" if PRIMARY == "cmd" => Some("primary"),
            "cmd" | "command" | "meta" | "super" | "win" => Some("meta"),
            "ctrl" | "control" if PRIMARY == "ctrl" => Some("primary"),
            "ctrl" | "control" => Some("ctrl"),
            "alt" | "option" | "opt" => Some("alt"),
            "shift" => Some("shift"),
            "fn" => Some("fn"),
            _ => None,
        };
        match modifier {
            Some("meta") => return None, // an unusual modifier: never reversible
            Some(m) => {
                let i = MODIFIERS.iter().position(|x| *x == m)?;
                modifiers[i] = true;
            }
            None => {
                if key.is_some() {
                    return None; // two non-modifier keys
                }
                key = Some(match part.as_str() {
                    "esc" => "escape".to_owned(),
                    "arrowup" | "uparrow" => "up".to_owned(),
                    "arrowdown" | "downarrow" => "down".to_owned(),
                    "arrowleft" | "leftarrow" => "left".to_owned(),
                    "arrowright" | "rightarrow" => "right".to_owned(),
                    "page_up" => "pageup".to_owned(),
                    "page_down" => "pagedown".to_owned(),
                    "plus" | "equal" => "=".to_owned(),
                    "minus" => "-".to_owned(),
                    "comma" => ",".to_owned(),
                    "leftbracket" => "[".to_owned(),
                    "rightbracket" => "]".to_owned(),
                    _ => part,
                });
            }
        }
    }
    let key = key?;
    let mut parts: Vec<&str> = MODIFIERS
        .iter()
        .zip(modifiers)
        .filter_map(|(m, on)| on.then_some(*m))
        .collect();
    parts.push(&key);
    Some(parts.join("+"))
}

/// What pressing `combo` may do. Only navigation, selection, copy and undo
/// shortcuts are reversible; `return` in a chat app sends.
#[must_use]
pub fn key_consequence(combo: &str) -> Consequence {
    match normalize_combo(combo) {
        Some(normalized) if REVERSIBLE_KEYS.contains(&normalized.as_str()) => {
            Consequence::Reversible
        }
        _ => Consequence::Commit,
    }
}

/// Words on a control that commits something.
const COMMITTING_WORDS: &[&str] = &[
    "send",
    "submit",
    "delete",
    "remove",
    "erase",
    "trash",
    "discard",
    "buy",
    "purchase",
    "pay",
    "checkout",
    "order",
    "subscribe",
    "unsubscribe",
    "confirm",
    "approve",
    "accept",
    "allow",
    "grant",
    "authorize",
    "post",
    "publish",
    "share",
    "reply",
    "forward",
    "transfer",
    "withdraw",
    "install",
    "uninstall",
    "update",
    "upgrade",
    "restart",
    "shutdown",
    "quit",
    "close",
    "save",
    "replace",
    "overwrite",
    "reset",
    "clear",
    "block",
    "report",
    "leave",
    "decline",
    "reject",
    "continue",
    "ok",
    "yes",
    "done",
    "finish",
    "apply",
    "merge",
    "deploy",
    "logout",
    "signout",
    "call",
];

/// Phrases split across words.
const COMMITTING_PHRASES: &[&str] = &["sign out", "log out", "shut down", "hang up", "end call"];

/// Roles whose activation is undone by activating them again.
const TOGGLE_ROLES: &[&str] = &[
    "checkbox",
    "switch",
    "radiobutton",
    "tab",
    "disclosuretriangle",
    "treeitem",
    "textfield",
    "textarea",
    "searchfield",
];

/// What clicking an element with `role` and accessible `name` may do.
/// Unnamed buttons count as commitments: nobody knows what they do.
#[must_use]
pub fn click_consequence(role: &str, name: Option<&str>) -> Consequence {
    let role = role.to_lowercase();
    if TOGGLE_ROLES.contains(&role.as_str()) {
        return Consequence::Reversible;
    }
    let Some(name) = name.map(str::to_lowercase).filter(|n| !n.trim().is_empty()) else {
        return Consequence::Commit;
    };
    let words: Vec<&str> = name
        .split(|c: char| !c.is_alphanumeric() && c != '\'')
        .filter(|w| !w.is_empty())
        .collect();
    let joined = words.join(" ");
    let commits = words.iter().any(|w| {
        // "deleting", "sends", "saved": match the stem.
        COMMITTING_WORDS
            .iter()
            .any(|c| *w == *c || (w.starts_with(c) && w.len() <= c.len() + 3))
    }) || COMMITTING_PHRASES.iter().any(|p| joined.contains(p))
        || name.contains("don't save");
    if commits {
        Consequence::Commit
    } else {
        Consequence::Reversible
    }
}

/// What typing `text` into a field may do. A newline can submit a chat box or
/// run a terminal command.
#[must_use]
pub fn typing_consequence(text: &str) -> Consequence {
    if text.contains(['\n', '\r']) {
        Consequence::Commit
    } else {
        Consequence::Reversible
    }
}

/// How the user answered "should I?".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Approval {
    /// Clearly yes.
    Approved,
    /// No, or any hesitation.
    Declined,
    /// Neither: they said something else.
    Unclear,
}

const NEGATIVE: &[&str] = &[
    "no",
    "nope",
    "nah",
    "don't",
    "dont",
    "not",
    "stop",
    "cancel",
    "wait",
    "never",
    "nevermind",
    "abort",
    "negative",
    "hold",
    "hang",
    // An amendment is not a yes: "yes, but say love from Grandma".
    "but",
    "change",
    "instead",
    "actually",
    "except",
    "rather",
];

const AFFIRMATIVE: &[&str] = &[
    "yes",
    "yeah",
    "yep",
    "yup",
    "yea",
    "sure",
    "ok",
    "okay",
    "confirm",
    "confirmed",
    "correct",
    "absolutely",
    "affirmative",
    "definitely",
    "proceed",
    "please",
    "alright",
    "fine",
    "right",
    "lovely",
    "perfect",
    "great",
    "good",
];

const AFFIRMATIVE_PHRASES: &[&str] = &[
    "go ahead",
    "do it",
    "go for it",
    "send it",
    "of course",
    "go on",
    "sounds good",
];

/// Reads the user's answer to a confirmation question.
///
/// Any hint of hesitation declines: "yes, but wait" is a no. This runs on
/// the user's own words, never on model output, so text on screen cannot
/// approve an action.
#[must_use]
pub fn approval(utterance: &str) -> Approval {
    let lowered = utterance.to_lowercase().replace('’', "'");
    let words: Vec<&str> = lowered
        .split(|c: char| !c.is_alphanumeric() && c != '\'')
        .filter(|w| !w.is_empty())
        .collect();
    if words
        .iter()
        .any(|w| NEGATIVE.contains(w) || w.ends_with("n't"))
    {
        return Approval::Declined;
    }
    let joined = words.join(" ");
    if words.iter().any(|w| AFFIRMATIVE.contains(w))
        || AFFIRMATIVE_PHRASES.iter().any(|p| joined.contains(p))
    {
        Approval::Approved
    } else {
        Approval::Unclear
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn primary(rest: &str) -> String {
        format!("{PRIMARY}+{rest}")
    }

    #[test]
    fn navigation_and_editing_shortcuts_are_reversible() {
        for combo in [
            "tab",
            "Shift+Tab",
            "esc",
            "down",
            &primary("t"),
            &primary("L"),
            &primary("c"),
            &primary("z"),
            &format!("shift+{PRIMARY}+z"),
            "ctrl+tab",
        ] {
            assert_eq!(key_consequence(combo), Consequence::Reversible, "{combo}");
        }
    }

    #[test]
    fn anything_that_might_submit_or_destroy_needs_confirmation() {
        for combo in [
            "return".to_owned(),
            "enter".to_owned(),
            "space".to_owned(),
            "delete".to_owned(),
            primary("return"),
            primary("s"),
            primary("w"),
            primary("q"),
            primary("v"),
            primary("delete"),
            primary("r"),
            "a+b".to_owned(),
            String::new(),
            "shift".to_owned(),
        ] {
            assert_eq!(key_consequence(&combo), Consequence::Commit, "{combo:?}");
        }
    }

    #[test]
    fn clicks_on_committing_controls_need_confirmation() {
        for name in [
            "Send",
            "Send now",
            "Delete",
            "Move to Trash",
            "Buy with 1-Click",
            "Sign Out",
            "Don't Save",
            "OK",
            "Allow",
            "Reply All",
            "Place your order",
        ] {
            assert_eq!(
                click_consequence("button", Some(name)),
                Consequence::Commit,
                "{name}"
            );
        }
        assert_eq!(click_consequence("button", None), Consequence::Commit);
        assert_eq!(click_consequence("button", Some("  ")), Consequence::Commit);
    }

    #[test]
    fn ordinary_clicks_run_at_once() {
        for (role, name) in [
            ("button", "New Document"),
            ("button", "Compose"),
            ("link", "Pricing"),
            ("button", "Bold"),
            ("checkbox", "Delete after upload"),
            ("tab", "Settings"),
            ("menuitem", "Show Sidebar"),
        ] {
            assert_eq!(
                click_consequence(role, Some(name)),
                Consequence::Reversible,
                "{role} {name}"
            );
        }
    }

    #[test]
    fn typing_a_newline_may_submit() {
        assert_eq!(typing_consequence("hello there"), Consequence::Reversible);
        assert_eq!(typing_consequence("ls\n"), Consequence::Commit);
        assert_eq!(typing_consequence("a\r"), Consequence::Commit);
    }

    #[test]
    fn policy_asks_before_commitments() {
        let p = ConfirmPolicy::Commits;
        assert_eq!(p.decide(Consequence::Observe), GateDecision::Act);
        assert_eq!(p.decide(Consequence::Reversible), GateDecision::Act);
        assert_eq!(p.decide(Consequence::Commit), GateDecision::Confirm);
        let strict = ConfirmPolicy::AllChanges;
        assert_eq!(strict.decide(Consequence::Observe), GateDecision::Act);
        assert_eq!(
            strict.decide(Consequence::Reversible),
            GateDecision::Confirm
        );
    }

    #[test]
    fn reads_spoken_answers() {
        for yes in [
            "Yes.",
            "yeah go ahead",
            "Sure",
            "Okay, do it.",
            "Yes please",
            "go for it",
            "That's fine, dear",
            "Oh lovely, yes",
            "Alright then",
            "Yes, that's right",
            "Sounds good",
        ] {
            assert_eq!(approval(yes), Approval::Approved, "{yes}");
        }
        for no in [
            "No.",
            "no, don't",
            "Wait",
            "Yes, but wait a second",
            "Actually don't send it",
            "Hold on",
            "I don't think so",
            "Cancel that",
            "Sure, oh no, never mind",
            "please don’t",
            "Yes, but say love from Grandma",
            "Okay, but change the subject",
            "Send it to Mary instead",
        ] {
            assert_eq!(approval(no), Approval::Declined, "{no}");
        }
        for unclear in ["", "What's the weather like?", "Open Safari"] {
            assert_eq!(approval(unclear), Approval::Unclear, "{unclear}");
        }
    }

    proptest! {
        #[test]
        fn any_negative_word_declines(
            before in "[a-z ,.]{0,30}",
            after in "[a-z ,.]{0,30}",
            negative in prop::sample::select(NEGATIVE),
        ) {
            let utterance = format!("yes {before} {negative} {after}");
            prop_assert_eq!(approval(&utterance), Approval::Declined);
        }

        #[test]
        fn key_classification_never_panics(combo in "\\PC{0,20}") {
            let _ = key_consequence(&combo);
        }
    }
}
