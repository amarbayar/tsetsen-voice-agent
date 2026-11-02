use async_trait::async_trait;

use crate::{AppId, Ctx, DesktopError, ElementRef, WindowId};

/// Where the user was when they pressed the hotkey.
///
/// Captured before any speech is processed, so "this" and plain dictation
/// resolve to what the user was looking at, not to whatever is in front when
/// the pipeline finishes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Anchor {
    /// The frontmost app.
    pub app: AppId,
    /// Its focused window.
    pub window: WindowId,
    /// The focused element, if it accepts text.
    pub element: Option<ElementRef>,
}

/// A window on screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowInfo {
    /// Stable for the window's lifetime.
    pub id: WindowId,
    /// The owning app's display name, e.g. `Google Chrome`.
    pub app: String,
    /// The window title. May be empty.
    pub title: String,
    /// Whether it has keyboard focus.
    pub focused: bool,
    /// Whether it is minimized.
    pub minimized: bool,
}

/// One element of a window's accessibility tree.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UiNode {
    /// Set when the element can be acted on.
    pub element: Option<ElementRef>,
    /// Platform-neutral role, e.g. `button`, `textfield`, `statictext`.
    pub role: String,
    /// Accessible name or label.
    pub name: Option<String>,
    /// Current value: field text, checkbox state, slider position.
    pub value: Option<String>,
    /// State tokens, e.g. `focused`, `checked`, `disabled`.
    pub states: Vec<String>,
    /// Actions it supports, e.g. `Click`, `TypeText`.
    pub actions: Vec<String>,
    /// A password or other secure field. Its value is never read.
    pub secure: bool,
    /// Child elements.
    pub children: Vec<UiNode>,
}

/// What a window contains, at one moment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowSnapshot {
    /// The window.
    pub window: WindowInfo,
    /// The root of its tree.
    pub root: UiNode,
    /// False if the platform cut the tree short (too deep or too slow).
    pub complete: bool,
}

/// One action against a pinned window.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum UiAction {
    /// Insert `text` at the element's caret, replacing any selection.
    /// Adapters pick the most reliable mechanism the platform offers.
    InsertText {
        /// The target field.
        element: ElementRef,
        /// The text to insert.
        text: String,
    },
    /// Press (activate) an element: a button, link, checkbox or menu item.
    Click {
        /// The target element.
        element: ElementRef,
    },
    /// Send a key combination to the window's app, e.g. `cmd+t`, `return`.
    PressKeys {
        /// Keys joined with `+`, modifiers first.
        combo: String,
    },
}

/// How [`UiAction::InsertText`] reaches a field.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TextEntry {
    /// As key presses: a newline is a Return key, which may submit.
    #[default]
    Keystrokes,
    /// Through the accessibility API, with no key events: a newline is just
    /// text.
    Inserted,
}

/// Whether the adapter confirmed the action's effect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    /// The effect was read back and matches.
    Verified,
    /// The action was sent, but its effect could not be confirmed.
    Unverified,
}

/// The result of a successful action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActionReport {
    /// Whether the effect was confirmed.
    pub delivery: Delivery,
}

/// Reads and operates desktop apps.
///
/// Every action names its window explicitly. There is deliberately no
/// "frontmost window" shortcut: the user may switch apps at any moment, and an
/// action must never land somewhere they did not intend.
#[async_trait]
pub trait Desktop: Send + Sync {
    /// How this adapter enters text. Assume key presses unless it knows
    /// better.
    fn text_entry(&self) -> TextEntry {
        TextEntry::Keystrokes
    }

    /// Display names of the apps that can be opened, e.g. `Google Chrome`.
    async fn installed_apps(&self, cx: &Ctx) -> Result<Vec<String>, DesktopError>;

    /// Launches `app` by display name, or brings it forward if it is
    /// running, and returns its front window.
    async fn open_app(&self, app: &str, cx: &Ctx) -> Result<WindowInfo, DesktopError>;

    /// Every visible window, front to back where the platform knows the order.
    async fn windows(&self, cx: &Ctx) -> Result<Vec<WindowInfo>, DesktopError>;

    /// Reads `window`'s contents. Element references are valid until the
    /// window changes.
    async fn snapshot(&self, window: &WindowId, cx: &Ctx) -> Result<WindowSnapshot, DesktopError>;

    /// Performs `action` in `window`.
    async fn act(
        &self,
        window: &WindowId,
        action: &UiAction,
        cx: &Ctx,
    ) -> Result<ActionReport, DesktopError>;
}
