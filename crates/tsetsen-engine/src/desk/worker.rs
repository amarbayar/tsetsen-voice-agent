//! A worker: one task, with its own context, doing the work.
//!
//! A task runs in segments. A segment starts when the task is started, told
//! something, or approved, and runs think-act steps until the worker has
//! something for the user: a result, a question, or an action that needs
//! their OK. Then it stops and reports to the desk. Its context stays with
//! the task, so the next segment continues where this one ended.
//!
//! Segments run under the task's own cancellation, not the conversation's:
//! interrupting what the front desk is saying never stops the work.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use futures_util::StreamExt;
use tsetsen_core::ports::{ThinkEvent, ThinkRequest, ToolCall, Turn};
use tsetsen_core::{Cancellation, Ctx, ProviderError, RequestId};
use tsetsen_policy::GateDecision;

use super::{Desk, Update, confirmation};
use crate::cancel::cancellable;
use crate::claims::{self, Claim};
use crate::conversation::{ActionOutcome, ActionRecord};
use crate::tools::ToolOutput;

/// An action held for the user's OK.
#[derive(Clone, Debug)]
pub(crate) struct Held {
    pub(crate) call: ToolCall,
    pub(crate) summary: String,
}

/// Where a task stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Status {
    /// A segment is running.
    Working,
    /// Waiting for the user: an answer, an OK, or a follow-up.
    Waiting,
    /// Stopped by the user; it takes nothing more.
    Stopped,
}

/// How a segment begins.
pub(crate) enum Start {
    /// From the task's context as it is: a brief or a message was added.
    Continue,
    /// The user approved the held action: run it, then continue.
    Approved,
}

/// One piece of work.
pub(crate) struct Task {
    pub(crate) id: String,
    turns: Mutex<Vec<Turn>>,
    held: Mutex<Option<Held>>,
    status: Mutex<Status>,
    cancel: Cancellation,
    /// Its approval request has been passed to the front desk.
    approval_delivered: AtomicBool,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Task {
    pub(crate) fn new(id: String, first: String) -> Self {
        Self {
            id,
            turns: Mutex::new(vec![Turn::user(first)]),
            held: Mutex::new(None),
            status: Mutex::new(Status::Working),
            cancel: Cancellation::new(),
            approval_delivered: AtomicBool::new(false),
        }
    }

    pub(crate) fn status(&self) -> Status {
        *lock(&self.status)
    }

    pub(crate) fn set_status(&self, status: Status) {
        *lock(&self.status) = status;
    }

    pub(crate) fn held(&self) -> Option<Held> {
        lock(&self.held).clone()
    }

    pub(crate) fn approval_delivered(&self) -> bool {
        self.approval_delivered.load(Ordering::Acquire)
    }

    pub(crate) fn mark_approval_delivered(&self) {
        self.approval_delivered.store(true, Ordering::Release);
    }

    /// Adds what the front desk passed on, for the next segment.
    pub(crate) fn tell(&self, text: String) {
        lock(&self.turns).push(Turn::user(text));
    }

    /// Drops the held action, recording why, so the worker's context stays
    /// consistent with what happened.
    pub(crate) fn withdraw(&self, why: String) -> Option<Held> {
        let held = lock(&self.held).take();
        self.approval_delivered.store(false, Ordering::Release);
        if held.is_some() {
            lock(&self.turns).push(Turn::user(why));
        }
        held
    }

    pub(crate) fn stop(&self) {
        self.cancel.cancel();
        self.set_status(Status::Stopped);
        lock(&self.held).take();
    }

    fn snapshot(&self) -> Vec<Turn> {
        lock(&self.turns).clone()
    }

    fn push(&self, turns: impl IntoIterator<Item = Turn>) {
        lock(&self.turns).extend(turns);
    }
}

/// What one segment produced.
pub(crate) struct Outcome {
    pub(crate) update: Update,
    pub(crate) actions: Vec<ActionRecord>,
}

/// Runs one segment of `task` to its end.
pub(crate) async fn run(desk: &Desk, task: &Task, start: Start, request: RequestId) -> Outcome {
    let cx = Ctx::new(request)
        .with_cancellation(task.cancel.clone())
        .with_timeout(desk.config.task_budget);
    let mut actions = Vec::new();
    // Taken first, so no lock is held across the run.
    let approved = match start {
        Start::Approved => lock(&task.held).take(),
        Start::Continue => None,
    };
    if let Some(held) = approved {
        task.approval_delivered.store(false, Ordering::Release);
        let call = ToolCall {
            id: format!("{}-approved", held.call.id),
            ..held.call
        };
        let (content, outcome) = run_tool(desk, &call, held.summary.clone(), &cx).await;
        actions.push(ActionRecord {
            summary: held.summary,
            outcome,
        });
        task.push([
            Turn::Assistant {
                text: String::new(),
                tool_calls: vec![call.clone()],
            },
            Turn::ToolResult {
                call_id: call.id,
                content: format!("The user approved. {content}"),
            },
        ]);
    }
    let update = steps(desk, task, &mut actions, &cx).await;
    Outcome { update, actions }
}

/// Think-act steps until there is something for the user.
async fn steps(desk: &Desk, task: &Task, actions: &mut Vec<ActionRecord>, cx: &Ctx) -> Update {
    let specs = desk.tools.specs();
    let mut nudged = false;
    let mut acted = !actions.is_empty();
    for index in 0..desk.config.max_steps.max(1) {
        if cx.check().is_err() {
            return Update::Stopped;
        }
        let request = ThinkRequest {
            must_use_tool: false,
            instructions: desk.worker_instructions(),
            turns: task.snapshot(),
            language: desk.language.clone(),
            tools: specs.clone(),
        };
        let (text, calls) = match think(desk, &request, cx).await {
            Ok(step) => step,
            Err(Some(problem)) => return Update::Failed(problem),
            Err(None) => return Update::Stopped,
        };
        task.push([Turn::Assistant {
            text: text.clone(),
            tool_calls: calls.clone(),
        }]);
        if calls.is_empty() {
            // "I've sent it" with nothing sent gets one correction.
            let claim = claims::claim(&text);
            let nudge = match claim {
                Claim::Completed if acted => None,
                other => claims::nudge(other),
            };
            if let Some(nudge) = nudge
                && !nudged
                && index + 1 < desk.config.max_steps
            {
                tracing::debug!(task = %task.id, ?claim, "words without actions; correcting");
                nudged = true;
                task.push([Turn::user(nudge)]);
                continue;
            }
            let report = text.trim();
            // Still claiming work that never ran, after the correction: the
            // claim is not passed on. "Sent" must never reach the user
            // unless something was sent.
            if !acted && claim == Claim::Completed {
                tracing::debug!(task = %task.id, "claimed work that never ran");
                return Update::Failed(
                    "it has not been done yet: the helper said it was, but nothing ran".into(),
                );
            }
            return if report.is_empty() {
                Update::Finished("Done.".into())
            } else if report.ends_with('?') {
                Update::Asking(report.to_owned())
            } else {
                Update::Finished(report.to_owned())
            };
        }
        let mut results = Vec::with_capacity(calls.len());
        for call in calls {
            let content = act(desk, task, &call, actions, cx).await;
            acted |= actions.last().is_some_and(|a| {
                matches!(
                    a.outcome,
                    ActionOutcome::Done | ActionOutcome::AwaitingApproval
                )
            });
            results.push(Turn::ToolResult {
                call_id: call.id,
                content,
            });
        }
        task.push(results);
        if let Some(held) = task.held() {
            return Update::NeedsApproval(held.summary);
        }
    }
    Update::Failed(format!(
        "stopped after {} steps without finishing",
        desk.config.max_steps
    ))
}

/// One thinker step: its text and tool calls. `Err(None)` if cancelled.
async fn think(
    desk: &Desk,
    request: &ThinkRequest,
    cx: &Ctx,
) -> Result<(String, Vec<ToolCall>), Option<String>> {
    let candidates = desk
        .thinker
        .candidates(desk.label, request.language.as_ref());
    if candidates.allowed.is_empty() {
        return Err(Some("no thinker may handle this task".into()));
    }
    let mut last = String::new();
    for thinker in candidates.allowed {
        let id = thinker.descriptor().id.clone();
        let mut stream = match cancellable(cx, thinker.think(request, cx)).await {
            Err(_) | Ok(Err(ProviderError::Interrupted(_))) => return Err(None),
            Ok(Ok(stream)) => stream,
            Ok(Err(error)) => {
                tracing::warn!(provider = %id, %error, "worker thinker failed to start");
                let fall_back = error.allows_fallback();
                last = format!("the thinker failed: {error}");
                if fall_back {
                    continue;
                }
                break;
            }
        };
        let (mut text, mut calls) = (String::new(), Vec::new());
        loop {
            match cancellable(cx, stream.next()).await {
                Ok(Some(Ok(ThinkEvent::Text(fragment)))) => text.push_str(&fragment),
                Ok(Some(Ok(ThinkEvent::ToolCall(call)))) => calls.push(call),
                Ok(Some(Err(error))) => {
                    return Err(Some(format!("the thinker failed mid-task: {error}")));
                }
                Ok(None) => return Ok((text, calls)),
                Err(_) => return Err(None),
            }
        }
    }
    Err(Some(last))
}

/// Assesses one call and runs it, or holds it for the user's OK. Returns
/// the result for the worker.
async fn act(
    desk: &Desk,
    task: &Task,
    call: &ToolCall,
    actions: &mut Vec<ActionRecord>,
    cx: &Ctx,
) -> String {
    if task.held().is_some() {
        return "Not run: waiting for the user's answer about the previous action.".into();
    }
    let assessment = match cancellable(cx, desk.tools.assess(call, cx)).await {
        Err(_) => return "Not run: the task was stopped.".into(),
        Ok(Err(problem)) => {
            actions.push(ActionRecord {
                summary: call.name.replace('_', " "),
                outcome: ActionOutcome::Failed(problem.clone()),
            });
            return problem;
        }
        Ok(Ok(assessment)) => assessment,
    };
    match desk.config.confirm.decide(assessment.consequence) {
        GateDecision::Act => {
            let progress = assessment.progress.clone();
            let (content, outcome) = run_tool(desk, call, progress.unwrap_or_default(), cx).await;
            actions.push(ActionRecord {
                summary: assessment.summary,
                outcome,
            });
            content
        }
        // Abstain cannot happen here; holding is the safe reading.
        GateDecision::Confirm | GateDecision::Abstain => {
            let content = format!(
                "Not done yet. The user is being asked: \"{}\" Stop here and write nothing \
                 more; their answer comes next.",
                confirmation(&assessment.summary)
            );
            *lock(&task.held) = Some(Held {
                call: call.clone(),
                summary: assessment.summary.clone(),
            });
            actions.push(ActionRecord {
                summary: assessment.summary,
                outcome: ActionOutcome::AwaitingApproval,
            });
            content
        }
    }
}

/// Runs a call, telling the desk what is happening while it does, and
/// turning cancellation into an honest "may have happened".
async fn run_tool(
    desk: &Desk,
    call: &ToolCall,
    progress: String,
    cx: &Ctx,
) -> (String, ActionOutcome) {
    let progress = (!progress.is_empty()).then_some(progress);
    desk.report_progress(progress);
    let result = cancellable(cx, desk.tools.run(call, cx)).await;
    desk.report_progress(None);
    match result {
        Ok(ToolOutput { content, ok: true }) => (content, ActionOutcome::Done),
        Ok(ToolOutput { content, ok: false }) => {
            let outcome = ActionOutcome::Failed(content.clone());
            (content, outcome)
        }
        Err(_) => (
            "Stopped while running: it may or may not have happened.".into(),
            ActionOutcome::Interrupted,
        ),
    }
}

/// Whether a segment may start: the task is not stopped and no segment is
/// running.
pub(crate) fn claim(task: &Arc<Task>) -> bool {
    let mut status = lock(&task.status);
    match *status {
        Status::Waiting => {
            *status = Status::Working;
            true
        }
        Status::Working | Status::Stopped => false,
    }
}
