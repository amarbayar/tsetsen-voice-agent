//! Against the real agent-desktop and real `TextEdit`. Needs an unlocked
//! screen and accessibility access for the terminal that runs the tests.
//!
//! ```sh
//! cargo test -p tsetsen-agent-desktop --test live -- --ignored --test-threads=1
//! ```
//!
//! `TSETSEN_AGENT_DESKTOP` overrides the binary path. The tests skip if
//! `TextEdit` is already running, so they never touch the user's documents,
//! and quit the `TextEdit` they started.

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use tsetsen_agent_desktop::{AgentDesktop, AgentDesktopConfig};
use tsetsen_core::ports::{Delivery, Desktop, UiAction, UiNode, WindowInfo};
use tsetsen_core::{Ctx, DesktopError, ElementRef, RequestId};

fn binary() -> PathBuf {
    std::env::var_os("TSETSEN_AGENT_DESKTOP").map_or_else(
        || {
            PathBuf::from(std::env::var_os("HOME").expect("HOME is set"))
                .join(".tsetsen/tools/agent-desktop-1.0.2/agent-desktop")
        },
        PathBuf::from,
    )
}

fn cx() -> Ctx {
    Ctx::new(RequestId::new(1)).with_timeout(Duration::from_secs(40))
}

async fn desktop() -> AgentDesktop {
    AgentDesktop::connect(AgentDesktopConfig::new(binary()))
        .await
        .expect("agent-desktop is installed and has accessibility access")
}

fn find<'a>(node: &'a UiNode, matches: &dyn Fn(&UiNode) -> bool) -> Option<&'a UiNode> {
    if matches(node) {
        return Some(node);
    }
    node.children.iter().find_map(|c| find(c, matches))
}

/// Quits `TextEdit` on drop, discarding the test document.
struct TextEdit;

impl Drop for TextEdit {
    fn drop(&mut self) {
        let _ = Command::new(binary())
            .args(["close-app", "TextEdit", "--force"])
            .output();
    }
}

/// Opens a fresh `TextEdit` document, or `None` if `TextEdit` was already
/// running (the user's documents are never touched).
async fn fresh_document(desktop: &AgentDesktop) -> Option<(TextEdit, WindowInfo)> {
    if desktop
        .windows(&cx())
        .await
        .unwrap()
        .iter()
        .any(|w| w.app == "TextEdit")
    {
        eprintln!("TextEdit is already running; skipping so as not to touch its documents");
        return None;
    }
    let guard = TextEdit;
    let window = desktop.open_app("TextEdit", &cx()).await.unwrap();
    // With iCloud, TextEdit opens a file panel first.
    let snapshot = desktop.snapshot(&window.id, &cx()).await.unwrap();
    let new_document = find(&snapshot.root, &|n| {
        n.role == "button" && n.name.as_deref() == Some("New Document")
    });
    if let Some(button) = new_document {
        let element = button.element.clone().unwrap();
        desktop
            .act(&window.id, &UiAction::Click { element }, &cx())
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(800)).await;
    }
    let document = desktop
        .windows(&cx())
        .await
        .unwrap()
        .into_iter()
        .find(|w| w.app == "TextEdit" && w.title.starts_with("Untitled"))
        .expect("a new document window");
    Some((guard, document))
}

fn text_area(root: &UiNode) -> ElementRef {
    find(root, &|n| {
        n.role == "textfield" && n.actions.iter().any(|a| a == "TypeText")
    })
    .and_then(|n| n.element.clone())
    .expect("the document's text area")
}

#[tokio::test]
#[ignore = "needs agent-desktop, accessibility access and an unlocked screen"]
async fn lists_installed_apps_and_windows() {
    let desktop = desktop().await;
    let apps = desktop.installed_apps(&cx()).await.unwrap();
    assert!(apps.iter().any(|a| a == "TextEdit"), "{apps:?}");
    let windows = desktop.windows(&cx()).await.unwrap();
    assert!(!windows.is_empty(), "at least this terminal is open");
    assert!(windows.iter().filter(|w| w.focused).count() <= 1);
}

#[tokio::test]
#[ignore = "needs agent-desktop, accessibility access and an unlocked screen"]
async fn types_into_a_document_and_reads_it_back() {
    let desktop = desktop().await;
    let Some((_quit, window)) = fresh_document(&desktop).await else {
        return;
    };
    let snapshot = desktop.snapshot(&window.id, &cx()).await.unwrap();
    let element = text_area(&snapshot.root);
    let text = "Hello from Tsetsen --force -5 degrees";
    let report = desktop
        .act(
            &window.id,
            &UiAction::InsertText {
                element: element.clone(),
                text: text.into(),
            },
            &cx(),
        )
        .await
        .unwrap();
    assert_eq!(report.delivery, Delivery::Verified);

    let after = desktop.snapshot(&window.id, &cx()).await.unwrap();
    let field = find(&after.root, &|n| n.role == "textfield").unwrap();
    // macOS may restore an earlier unsaved document, so only the end is ours.
    assert!(
        field.value.as_deref().unwrap_or_default().ends_with(text),
        "dashes typed, not parsed: {:?}",
        field.value
    );

    // Select all with a real keystroke, then replace the selection.
    desktop
        .act(
            &window.id,
            &UiAction::PressKeys {
                combo: "cmd+a".into(),
            },
            &cx(),
        )
        .await
        .unwrap();
    let element = text_area(&desktop.snapshot(&window.id, &cx()).await.unwrap().root);
    desktop
        .act(
            &window.id,
            &UiAction::InsertText {
                element,
                text: "Replaced.".into(),
            },
            &cx(),
        )
        .await
        .unwrap();
    let last = desktop.snapshot(&window.id, &cx()).await.unwrap();
    let field = find(&last.root, &|n| n.role == "textfield").unwrap();
    assert_eq!(field.value.as_deref(), Some("Replaced."));

    // Leave it empty, so macOS has nothing to restore next launch.
    for combo in ["cmd+a", "delete"] {
        desktop
            .act(
                &window.id,
                &UiAction::PressKeys {
                    combo: combo.into(),
                },
                &cx(),
            )
            .await
            .unwrap();
    }
}

#[tokio::test]
#[ignore = "needs agent-desktop, accessibility access and an unlocked screen"]
async fn reports_unknown_apps_and_stale_elements_precisely() {
    let desktop = desktop().await;
    let error = desktop
        .open_app("No Such App 4f2a", &cx())
        .await
        .unwrap_err();
    assert!(matches!(error, DesktopError::AppNotFound(_)), "{error:?}");

    let Some((_quit, window)) = fresh_document(&desktop).await else {
        return;
    };
    let error = desktop
        .act(
            &window.id,
            &UiAction::Click {
                element: ElementRef::new("@s0000000000:e1"),
            },
            &cx(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, DesktopError::StaleElement(_)), "{error:?}");
}

#[tokio::test]
#[ignore = "needs agent-desktop and accessibility access"]
async fn honors_cancellation() {
    let desktop = desktop().await;
    let cancel = tsetsen_core::Cancellation::new();
    cancel.cancel();
    let cx = cx().with_cancellation(cancel);
    let error = desktop.windows(&cx).await.unwrap_err();
    assert!(matches!(error, DesktopError::Interrupted(_)), "{error:?}");
}
