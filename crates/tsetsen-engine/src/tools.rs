//! Tools the thinker can call, behind one interface.
//!
//! A toolbox says what a call *would* do before doing it, so the engine can
//! hold commitments for the user's spoken OK. Desktop control is one toolbox;
//! others (calendar, files, web) plug in the same way.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use tsetsen_core::Ctx;
use tsetsen_core::ports::{ToolCall, ToolSpec};
use tsetsen_policy::Consequence;

/// What a call would do, judged before it runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assessment {
    /// A short, speakable description: "press Return in Slack".
    pub summary: String,
    /// What it may do to the user's world.
    pub consequence: Consequence,
    /// What to tell the user if it takes a while: "Looking through your
    /// pictures". `None` for calls too quick to mention.
    pub progress: Option<String>,
}

/// The result of running a call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolOutput {
    /// Written for the model.
    pub content: String,
    /// Whether the call did what it was asked.
    pub ok: bool,
}

impl ToolOutput {
    /// A success.
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            ok: true,
        }
    }

    /// A failure, explained for the model.
    pub fn failed(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            ok: false,
        }
    }
}

/// A set of tools.
#[async_trait]
pub trait Toolbox: Send + Sync {
    /// The tools, as offered to the thinker.
    fn specs(&self) -> Vec<ToolSpec>;

    /// Validates `call` and says what it would do, without doing it. An
    /// error is explained for the model: unknown tool, bad arguments, no
    /// such element.
    async fn assess(&self, call: &ToolCall, cx: &Ctx) -> Result<Assessment, String>;

    /// Runs `call`. Failures are reported in the output, not as errors: the
    /// model needs to hear about them to recover.
    async fn run(&self, call: &ToolCall, cx: &Ctx) -> ToolOutput;

    /// What this toolbox has found so far, written for the model, so a new
    /// task can build on earlier ones. `None` if there is nothing.
    fn context(&self) -> Option<String> {
        None
    }
}

/// Two toolboxes offered the same tool name.
#[derive(Debug, thiserror::Error)]
#[error("more than one toolbox offers `{0}`")]
pub struct DuplicateTool(pub String);

/// Several toolboxes offered as one. Calls go to the toolbox that offers the
/// tool.
pub struct ToolSet {
    boxes: Vec<Arc<dyn Toolbox>>,
    owner: HashMap<String, usize>,
}

impl ToolSet {
    /// Combines `boxes`. Tool names must be unique across them.
    pub fn new(boxes: Vec<Arc<dyn Toolbox>>) -> Result<Self, DuplicateTool> {
        let mut owner = HashMap::new();
        for (index, toolbox) in boxes.iter().enumerate() {
            for spec in toolbox.specs() {
                if owner.insert(spec.name.clone(), index).is_some() {
                    return Err(DuplicateTool(spec.name));
                }
            }
        }
        Ok(Self { boxes, owner })
    }

    fn owner(&self, call: &ToolCall) -> Option<&dyn Toolbox> {
        self.owner.get(&call.name).map(|&i| self.boxes[i].as_ref())
    }
}

fn unknown(call: &ToolCall) -> String {
    format!("There is no tool named `{}`.", call.name)
}

#[async_trait]
impl Toolbox for ToolSet {
    fn specs(&self) -> Vec<ToolSpec> {
        self.boxes.iter().flat_map(|b| b.specs()).collect()
    }

    async fn assess(&self, call: &ToolCall, cx: &Ctx) -> Result<Assessment, String> {
        match self.owner(call) {
            Some(toolbox) => toolbox.assess(call, cx).await,
            None => Err(unknown(call)),
        }
    }

    async fn run(&self, call: &ToolCall, cx: &Ctx) -> ToolOutput {
        match self.owner(call) {
            Some(toolbox) => toolbox.run(call, cx).await,
            None => ToolOutput::failed(unknown(call)),
        }
    }

    fn context(&self) -> Option<String> {
        let parts: Vec<String> = self.boxes.iter().filter_map(|b| b.context()).collect();
        (!parts.is_empty()).then(|| parts.join("\n"))
    }
}
