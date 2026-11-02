//! agent-desktop's JSON to the port's types.

use serde_json::Value;
use tsetsen_core::ports::{Delivery, UiNode, WindowInfo, WindowSnapshot};
use tsetsen_core::{DesktopError, ElementRef, WindowId};

/// Roles that hold secrets. agent-desktop already redacts secure text; this
/// is a second line, so a secret never reaches a model even if it did not.
const SECURE_ROLES: &[&str] = &["securetextfield", "passwordfield", "password"];

/// Deeper trees are cut: a runaway tree must not exhaust memory or context.
const MAX_DEPTH: usize = 64;

fn text(value: &Value, key: &str) -> Option<String> {
    match value.get(key)? {
        Value::String(s) => Some(s.clone()),
        Value::Null => None,
        other => Some(other.to_string()),
    }
}

fn strings(value: &Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn window(value: &Value) -> Result<WindowInfo, DesktopError> {
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| DesktopError::Platform("window without an id".into()))?;
    Ok(WindowInfo {
        id: WindowId::new(id),
        app: text(value, "app_name").unwrap_or_default(),
        title: text(value, "title").unwrap_or_default(),
        focused: value
            .get("is_focused")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        minimized: value
            .get("minimized")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

/// Visible windows, focused first.
pub(crate) fn windows(data: &Value) -> Result<Vec<WindowInfo>, DesktopError> {
    let mut windows = data
        .as_array()
        .ok_or_else(|| DesktopError::Platform("window list is not a list".into()))?
        .iter()
        .filter(|w| w.get("visible").and_then(Value::as_bool) != Some(false))
        .map(window)
        .collect::<Result<Vec<_>, _>>()?;
    windows.sort_by_key(|w| !w.focused); // stable: platform order otherwise
    Ok(windows)
}

fn node(value: &Value, depth: usize) -> UiNode {
    let role = text(value, "role").unwrap_or_default();
    let states = strings(value, "states");
    let secure = SECURE_ROLES.contains(&role.to_lowercase().as_str())
        || states.iter().any(|s| s == "protected" || s == "secure");
    let children = if depth >= MAX_DEPTH {
        Vec::new()
    } else {
        value
            .get("children")
            .and_then(Value::as_array)
            .map(|c| c.iter().map(|child| node(child, depth + 1)).collect())
            .unwrap_or_default()
    };
    UiNode {
        element: text(value, "ref_id").map(ElementRef::new),
        role,
        name: text(value, "name"),
        value: if secure { None } else { text(value, "value") },
        states,
        actions: strings(value, "available_actions"),
        secure,
        children,
    }
}

pub(crate) fn snapshot(
    data: &Value,
    fallback: &WindowInfo,
) -> Result<WindowSnapshot, DesktopError> {
    let tree = data
        .get("tree")
        .ok_or_else(|| DesktopError::Platform("snapshot without a tree".into()))?;
    let mut window = fallback.clone();
    if let Some(title) = data.get("window").and_then(|w| text(w, "title")) {
        window.title = title;
    }
    Ok(WindowSnapshot {
        window,
        root: node(tree, 0),
        complete: data
            .get("complete")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

/// Whether the effect was observed.
pub(crate) fn delivery(data: &Value) -> Delivery {
    match data
        .pointer("/disposition/delivery")
        .and_then(Value::as_str)
    {
        Some("delivered_verified") => Delivery::Verified,
        _ => Delivery::Unverified,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn converts_a_snapshot_and_withholds_secure_values() {
        let data = json!({
            "app": "Mail", "complete": true, "snapshot_id": "s1",
            "window": {"id": "w-9", "title": "Inbox"},
            "tree": {"role": "window", "name": "Inbox", "children": [
                {"role": "button", "name": "Send", "ref_id": "@s1:e1",
                 "available_actions": ["Click"], "states": []},
                {"role": "securetextfield", "name": "Password", "value": "hunter2",
                 "ref_id": "@s1:e2", "available_actions": ["SetValue"]},
                {"role": "scrollbar", "value": 0.25}
            ]}
        });
        let fallback = WindowInfo {
            id: WindowId::new("w-9"),
            app: "Mail".into(),
            title: String::new(),
            focused: true,
            minimized: false,
        };
        let snapshot = snapshot(&data, &fallback).unwrap();
        assert_eq!(snapshot.window.title, "Inbox");
        assert!(snapshot.complete);
        let [send, password, scrollbar] = &snapshot.root.children[..] else {
            panic!()
        };
        assert_eq!(send.element, Some(ElementRef::new("@s1:e1")));
        assert_eq!(send.actions, ["Click"]);
        assert!(password.secure && password.value.is_none());
        assert_eq!(scrollbar.value.as_deref(), Some("0.25"));
    }

    #[test]
    fn lists_visible_windows_focused_first() {
        let data = json!([
            {"id": "w-1", "app_name": "Notes", "title": "A", "is_focused": false, "visible": true},
            {"id": "w-2", "app_name": "Mail", "title": "B", "is_focused": true},
            {"id": "w-3", "app_name": "Hidden", "title": "C", "visible": false}
        ]);
        let listed = windows(&data).unwrap();
        assert_eq!(
            listed.iter().map(|w| w.id.as_str()).collect::<Vec<_>>(),
            ["w-2", "w-1"]
        );
        assert!(windows(&json!({})).is_err());
    }

    #[test]
    fn cuts_runaway_trees() {
        let mut deep = json!({"role": "leaf"});
        for _ in 0..200 {
            deep = json!({"role": "group", "children": [deep]});
        }
        let root = node(&deep, 0);
        let mut depth = 0;
        let mut at = &root;
        while let Some(child) = at.children.first() {
            at = child;
            depth += 1;
        }
        assert_eq!(depth, MAX_DEPTH);
    }

    #[test]
    fn delivery_is_verified_only_when_observed() {
        assert_eq!(
            delivery(&json!({"disposition": {"delivery": "delivered_verified"}})),
            Delivery::Verified
        );
        assert_eq!(
            delivery(&json!({"disposition": {"delivery": "delivered_unverified"}})),
            Delivery::Unverified
        );
        assert_eq!(delivery(&json!({})), Delivery::Unverified);
    }
}
