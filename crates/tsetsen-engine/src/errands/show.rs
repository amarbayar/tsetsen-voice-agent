//! Showing things on screen, numbered, so the user can see what is meant:
//! pictures, and documents that can be previewed.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use tsetsen_core::Ctx;
use tsetsen_core::ports::{ToolCall, ToolSpec};
use tsetsen_policy::Consequence;

use super::{Handles, MAX_SHOWN, Shared, args, spec};
use crate::tools::{Assessment, ToolOutput, Toolbox};

const SHOW: &str = "show";
/// What the screen can preview.
const PREVIEWABLE: &[&str] = &[
    "jpg", "jpeg", "png", "heic", "gif", "tif", "tiff", "bmp", "webp", "pdf",
];

pub(crate) fn previewable(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| PREVIEWABLE.contains(&e.to_ascii_lowercase().as_str()))
}

/// Shows `paths` if this computer can; returns whether it did.
pub(crate) async fn present(shared: &Shared, paths: &[PathBuf], cx: &Ctx) -> bool {
    let Some(showcase) = &shared.caps.showcase else {
        return false;
    };
    match showcase.show(paths, cx).await {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(%error, "could not show");
            false
        }
    }
}

/// Takes whatever is shown off the screen.
pub(crate) async fn clear(shared: &Shared, cx: &Ctx) {
    if let Some(showcase) = &shared.caps.showcase
        && let Err(error) = showcase.hide(cx).await
    {
        tracing::warn!(%error, "could not hide");
    }
}

/// Showing things the conversation has found.
pub(crate) struct ShowTools(Arc<Shared>);

pub(crate) fn offer(shared: &Arc<Shared>) -> Option<Arc<dyn Toolbox>> {
    let can_find = shared.caps.photos.is_some() || shared.caps.files.is_some();
    (shared.caps.showcase.is_some() && can_find)
        .then(|| Arc::new(ShowTools(Arc::clone(shared))) as Arc<dyn Toolbox>)
}

#[derive(Deserialize)]
struct ShowArgs {
    #[serde(alias = "item", alias = "ids")]
    items: Handles,
}

impl ShowTools {
    fn plan(&self, call: &ToolCall) -> Result<Vec<PathBuf>, String> {
        let ShowArgs { items } = args(call)?;
        let handles = items.into_vec();
        if handles.is_empty() {
            return Err("Say what to show, with ids like p1 or f2.".into());
        }
        if handles.len() > MAX_SHOWN {
            return Err(format!("At most {MAX_SHOWN} at once."));
        }
        handles
            .iter()
            .map(|h| {
                let item = self.0.items.get(h).ok_or_else(|| self.0.items.unknown(h))?;
                let (path, _) = item.file().ok_or_else(|| {
                    format!("`{h}` is a person; only pictures and files can be shown.")
                })?;
                if previewable(&path) {
                    Ok(path)
                } else {
                    Err(format!(
                        "`{h}` cannot be previewed on screen; it can be opened instead."
                    ))
                }
            })
            .collect()
    }
}

#[async_trait]
impl Toolbox for ShowTools {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![spec(
            SHOW,
            "Show pictures or documents on the screen, numbered in the order given, so the user \
             can see which ones are meant.",
            r#"{"type":"object","properties":{"items":{"type":"array","items":{"type":"string"},"description":"Ids like p1 or f2"}},"required":["items"]}"#,
        )]
    }

    async fn assess(&self, call: &ToolCall, _cx: &Ctx) -> Result<Assessment, String> {
        let paths = self.plan(call)?;
        Ok(Assessment {
            summary: format!(
                "show {} on the screen",
                super::plural(paths.len(), "item", "items")
            ),
            consequence: Consequence::Observe,
            progress: None,
        })
    }

    async fn run(&self, call: &ToolCall, cx: &Ctx) -> ToolOutput {
        let paths = match self.plan(call) {
            Ok(paths) => paths,
            Err(problem) => return ToolOutput::failed(problem),
        };
        if present(&self.0, &paths, cx).await {
            ToolOutput::ok(format!(
                "On the screen now, numbered 1 to {} in the order given.",
                paths.len()
            ))
        } else {
            ToolOutput::failed("They could not be shown on the screen.")
        }
    }
}
