use crate::session::{Session, SessionEvent};
use crate::task::EventStore;
use crate::verification::verify_goal;
use crate::worker::RunLease;
use crate::{
    compact_tool_description, compact_tool_result, compact_tool_schema, select_tools_for_query,
};
use crate::{ApprovalScope, Runtime};
use anyhow::Context;
use aris_llm::{FinishReason, LlmEvent, LlmRequestOptions, ToolFunctionSpec, ToolSpec};
use aris_protocol::{
    CheckpointRecord, EventEnvelope, EventSource, GoalSpec, RetryPolicy, RunState, TaskRecord,
    TaskState,
};
use aris_tools::{ToolCall, ToolContext, ToolPipeline};
use chrono::Utc;
use parking_lot::RwLock;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub enum AgentEvent {
    TurnStarted(String),
    TurnEnded(String),
    TextDelta(String),
    ReasoningDelta(String),
    ThoughtTick {
        t: usize,
        note: String,
    },
    ToolStarted {
        name: String,
        call_id: String,
    },
    ToolFinished {
        name: String,
        call_id: String,
        ok: bool,
        preview: String,
    },
    /// Codex on-request / untrusted: wait for human y/n before running the tool.
    ApprovalNeeded {
        request_id: String,
        call_id: String,
        name: String,
        summary: String,
    },
    Error(String),
    Done,
}

/// Correlation context for events emitted to a frontend transport.
///
/// The live UI event enum intentionally stays compact for existing TUI
/// callers.  This context lets CLI, App Server, and MCP adapters project the
/// same event into the versioned protocol envelope without duplicating the
/// execution loop.
#[derive(Debug, Clone, Default)]
pub struct AgentEventContext {
    pub session_id: Option<String>,
    pub task_id: Option<String>,
    pub run_id: Option<String>,
    pub turn_id: Option<String>,
    pub step_id: Option<String>,
}

impl AgentEventContext {
    pub fn for_session(session_id: impl Into<String>) -> Self {
        Self {
            session_id: Some(session_id.into()),
            ..Self::default()
        }
    }
}

impl AgentEvent {
    /// Convert a live agent event to the stable transport envelope.
    ///
    /// `sequence` is assigned by the event store when the adapter persists the
    /// envelope.  A zero sequence therefore means a live event could not be
    /// persisted, while the schema and correlation fields remain available to
    /// the caller.
    pub fn to_protocol_event(&self, context: &AgentEventContext) -> EventEnvelope {
        let (event_type, source, payload) = match self {
            Self::TurnStarted(id) => (
                "agent.turn_started",
                EventSource::Core,
                json!({"turn_id": id}),
            ),
            Self::TurnEnded(id) => (
                "agent.turn_ended",
                EventSource::Core,
                json!({"turn_id": id}),
            ),
            Self::TextDelta(text) => (
                "agent.text_delta",
                EventSource::Model,
                json!({"text": text}),
            ),
            Self::ReasoningDelta(text) => (
                "agent.reasoning_delta",
                EventSource::Model,
                json!({"text": text}),
            ),
            Self::ThoughtTick { t, note } => (
                "agent.thought_tick",
                EventSource::Core,
                json!({"t": t, "note": note}),
            ),
            Self::ToolStarted { name, call_id } => (
                "agent.tool_started",
                EventSource::Tool,
                json!({"name": name, "call_id": call_id}),
            ),
            Self::ToolFinished {
                name,
                call_id,
                ok,
                preview,
            } => (
                "agent.tool_finished",
                EventSource::Tool,
                json!({"name": name, "call_id": call_id, "ok": ok, "preview": preview}),
            ),
            Self::ApprovalNeeded {
                request_id,
                call_id,
                name,
                summary,
            } => (
                "agent.approval_needed",
                EventSource::Core,
                json!({"request_id": request_id, "call_id": call_id, "name": name, "summary": summary}),
            ),
            Self::Error(message) => (
                "agent.error",
                EventSource::System,
                json!({"message": message}),
            ),
            Self::Done => ("agent.done", EventSource::Core, json!({})),
        };

        let mut event = EventEnvelope::new(event_type, payload).with_source(source);
        if let Some(id) = &context.task_id {
            event = event.with_task(id.clone());
        }
        if let Some(id) = &context.run_id {
            event = event.with_run(id.clone());
        }
        let turn_id = context.turn_id.clone().or_else(|| match self {
            Self::TurnStarted(id) | Self::TurnEnded(id) => Some(id.clone()),
            _ => None,
        });
        if let Some(id) = turn_id {
            event = event.with_turn(id);
        }
        if let Some(id) = &context.step_id {
            event = event.with_step(id.clone());
        }
        if let Some(id) = &context.session_id {
            event.payload["session_id"] = json!(id);
        }
        event
    }
}

#[derive(Clone)]
pub struct AgentHandle {
    pub session: Arc<RwLock<Session>>,
    cancel_tx: watch::Sender<bool>,
    pause_tx: watch::Sender<bool>,
}

impl AgentHandle {
    pub fn cancel(&self) {
        let _ = self.cancel_tx.send(true);
    }

    pub fn pause(&self) {
        let _ = self.pause_tx.send(true);
    }
}

pub struct AgentLoop {
    runtime: Arc<Runtime>,
    profile: Option<crate::kei::ModelProfileConfig>,
    execution_limits: Option<(usize, u32, usize)>,
}

struct TurnControl {
    cancel_rx: watch::Receiver<bool>,
    pause_rx: watch::Receiver<bool>,
    handle: AgentHandle,
    execution_limits: Option<(usize, u32, usize)>,
}

struct TurnModelIdentity {
    profile_id: Option<String>,
    model_id: Option<String>,
    family: Option<String>,
}

impl AgentLoop {
    pub fn new(runtime: Arc<Runtime>) -> Self {
        Self {
            runtime,
            profile: None,
            execution_limits: None,
        }
    }

    pub fn with_profile(
        runtime: Arc<Runtime>,
        profile: crate::kei::ModelProfileConfig,
    ) -> anyhow::Result<Self> {
        let gateway = crate::kei::load_kei_config(&runtime.outer_home, &runtime.config.kei)?;
        profile.resolve(&gateway, &runtime.llm.config(), &runtime.outer_home)?;
        Ok(Self {
            runtime,
            profile: Some(profile),
            execution_limits: None,
        })
    }

    /// Limits are local to this invocation, never global model/user settings.
    pub fn with_execution_limits(mut self, steps: usize, tokens: u32, tools: usize) -> Self {
        self.execution_limits = Some((steps.max(1), tokens.max(1), tools.max(1)));
        self
    }

    pub async fn run_turn(
        &self,
        session: Arc<RwLock<Session>>,
        user_text: String,
        event_tx: mpsc::Sender<AgentEvent>,
    ) -> anyhow::Result<AgentHandle> {
        anyhow::ensure!(
            !self.runtime.tasks.is_team_session(&session.read().id)
                && !self.runtime.teams.owns_session(&session.read().id)?,
            "Team conversations must be continued through teams/message or teams/admin"
        );
        self.spawn_turn(session, user_text, event_tx, None).await
    }

    /// Start a turn against a scheduler-reserved durable task.
    pub async fn run_task(
        &self,
        task_id: impl Into<String>,
        session: Arc<RwLock<Session>>,
        user_text: String,
        event_tx: mpsc::Sender<AgentEvent>,
    ) -> anyhow::Result<AgentHandle> {
        let task_id = task_id.into();
        anyhow::ensure!(
            self.runtime
                .tasks
                .task(&task_id)
                .is_none_or(|task| task.owner.is_none()),
            "Team-owned tasks must be resumed through their team coordinator"
        );
        anyhow::ensure!(
            !self.runtime.tasks.is_team_session(&session.read().id)
                && !self.runtime.teams.owns_session(&session.read().id)?,
            "Team conversations must be continued through their team coordinator"
        );
        self.spawn_reserved_task(task_id, session, user_text, event_tx)
            .await
    }

    /// Only the dependency coordinator can invoke a registered subagent. The
    /// profile and conversation come from the persisted role, never a caller.
    pub(crate) async fn run_team_task(
        &self,
        task_id: String,
        session: Arc<RwLock<Session>>,
        user_text: String,
        event_tx: mpsc::Sender<AgentEvent>,
    ) -> anyhow::Result<AgentHandle> {
        let task = self
            .runtime
            .tasks
            .task(&task_id)
            .context("team task missing")?;
        let owner = task.owner.as_ref().context("team task ownership missing")?;
        let team = self.runtime.teams.get(&owner.team_id)?;
        let agent = team
            .agents
            .iter()
            .find(|agent| agent.id == owner.agent_id)
            .context("subagent missing")?;
        anyhow::ensure!(
            team.status == "running" && agent.status == "running",
            "subagent is not scheduled"
        );
        anyhow::ensure!(
            agent.subagent_id.as_deref() == Some(&owner.subagent_id)
                && agent.task_id.as_deref() == Some(&task_id)
                && task.session_id.as_deref() == Some(&agent.session_id)
                && session.read().id == agent.session_id,
            "subagent task/session binding mismatch"
        );
        anyhow::ensure!(
            self.profile
                .as_ref()
                .is_some_and(|profile| profile.id == agent.profile_id),
            "subagent must use its assigned model profile"
        );
        anyhow::ensure!(
            std::path::Path::new(&owner.workspace).canonicalize()?
                == self.runtime.workspace_root.canonicalize()?,
            "subagent workspace mismatch"
        );
        self.spawn_reserved_task(task_id, session, user_text, event_tx)
            .await
    }

    async fn spawn_reserved_task(
        &self,
        task_id: String,
        session: Arc<RwLock<Session>>,
        user_text: String,
        event_tx: mpsc::Sender<AgentEvent>,
    ) -> anyhow::Result<AgentHandle> {
        self.runtime.reserve_task(task_id.clone());
        match self
            .spawn_turn(session, user_text, event_tx, Some(task_id.clone()))
            .await
        {
            Ok(handle) => Ok(handle),
            Err(error) => {
                self.runtime.release_task_reservation(&task_id);
                Err(error)
            }
        }
    }

    async fn spawn_turn(
        &self,
        session: Arc<RwLock<Session>>,
        user_text: String,
        event_tx: mpsc::Sender<AgentEvent>,
        task_override: Option<String>,
    ) -> anyhow::Result<AgentHandle> {
        // Freeze the client and prompt before spawning. Updating another agent's
        // settings cannot alter this turn, including subsequent tool-loop steps.
        let base_config = self.runtime.llm.config();
        let mut llm_config = if let Some(profile) = &self.profile {
            let gateway =
                crate::kei::load_kei_config(&self.runtime.outer_home, &self.runtime.config.kei)?;
            profile.resolve(&gateway, &base_config, &self.runtime.outer_home)?
        } else {
            base_config
        };
        if let Some((_, tokens, _)) = self.execution_limits {
            llm_config.max_tokens = llm_config.max_tokens.min(tokens).max(1);
        }
        let model_identity = TurnModelIdentity {
            profile_id: self.profile.as_ref().map(|profile| profile.id.clone()),
            model_id: (!llm_config.model.is_empty()).then(|| llm_config.model.clone()),
            family: self
                .profile
                .as_ref()
                .map(|profile| profile.family.id().into()),
        };
        let turn_llm = Arc::new(aris_llm::OpenAiCompatibleClient::new(llm_config)?);
        turn_llm.set_send_local_api_key(self.runtime.llm.sends_local_api_key());
        let mut turn_prompt = self.runtime.prompt.read().clone();
        if let Some(profile) = &self.profile {
            turn_prompt.set_section("agent_role", profile.instructions.clone());
        }
        let vision = self
            .profile
            .as_ref()
            .map(|profile| profile.vision)
            .unwrap_or_else(|| {
                crate::kei::load_kei_config(&self.runtime.outer_home, &self.runtime.config.kei)
                    .ok()
                    .and_then(|config| {
                        config.profiles.into_iter().find(|profile| {
                            profile.model == turn_llm.config().model
                                && profile.base_url.as_deref().unwrap_or(&config.base_url)
                                    == turn_llm.config().base_url
                        })
                    })
                    .is_some_and(|profile| profile.vision)
            });
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let (pause_tx, pause_rx) = watch::channel(false);
        let handle = AgentHandle {
            session: session.clone(),
            cancel_tx,
            pause_tx,
        };

        let runtime = self.runtime.clone();
        let runtime_for_release = runtime.clone();
        let control_handle = handle.clone();
        let reservation_id = task_override.clone();
        let execution_limits = self.execution_limits;
        tokio::spawn(async move {
            if let Err(err) = run_turn_inner(
                runtime,
                session,
                user_text,
                event_tx.clone(),
                task_override,
                turn_llm,
                turn_prompt,
                vision,
                model_identity,
                TurnControl {
                    cancel_rx,
                    pause_rx,
                    handle: control_handle,
                    execution_limits,
                },
            )
            .await
            {
                let _ = event_tx.send(AgentEvent::Error(err.to_string())).await;
            }
            if let Some(task_id) = reservation_id {
                runtime_for_release.release_task_reservation(&task_id);
            }
            let _ = event_tx.send(AgentEvent::Done).await;
        });

        Ok(handle)
    }
}

async fn run_turn_inner(
    runtime: Arc<Runtime>,
    session: Arc<RwLock<Session>>,
    user_text: String,
    event_tx: mpsc::Sender<AgentEvent>,
    task_override: Option<String>,
    turn_llm: Arc<aris_llm::OpenAiCompatibleClient>,
    mut turn_prompt: crate::SystemPromptBuilder,
    vision: bool,
    model_identity: TurnModelIdentity,
    control: TurnControl,
) -> anyhow::Result<()> {
    let TurnControl {
        mut cancel_rx,
        pause_rx,
        handle: control_handle,
        execution_limits,
    } = control;
    let turn_id = Uuid::new_v4().to_string();
    let (session_id, session_goal, goal_paused) = {
        let current = session.read();
        (
            current.id.clone(),
            current.goal.clone(),
            current.goal_paused,
        )
    };
    if goal_paused && session_goal.is_some() {
        anyhow::bail!("session goal is paused; use /goal resume before running it");
    }
    let goal_text = session_goal
        .filter(|goal| !goal.trim().is_empty())
        .unwrap_or_else(|| {
            if user_text.trim().is_empty() {
                "interactive turn".to_string()
            } else {
                user_text.clone()
            }
        });
    let task_id = if let Some(task_id) = task_override {
        let task = runtime
            .tasks
            .task(&task_id)
            .ok_or_else(|| anyhow::anyhow!("task not found: {task_id}"))?;
        if task.session_id.as_deref() != Some(session_id.as_str()) {
            anyhow::bail!("task {task_id} is not attached to session {session_id}");
        }
        if matches!(task.state, TaskState::Completed | TaskState::Cancelled) {
            anyhow::bail!("task {task_id} is terminal ({:?})", task.state);
        }
        if task.state == TaskState::Failed {
            runtime.tasks.transition_task(&task.id, TaskState::Queued)?;
        }
        if matches!(
            runtime.tasks.task(&task.id).map(|task| task.state),
            Some(TaskState::Queued | TaskState::Paused)
        ) {
            runtime
                .tasks
                .transition_task(&task.id, TaskState::Running)?;
        }
        task_id
    } else {
        let reusable_task = runtime
            .tasks
            .task_for_session(&session_id)
            .filter(|task| task.goal.outcome == goal_text)
            .filter(|task| {
                matches!(
                    task.state,
                    TaskState::Queued | TaskState::Paused | TaskState::Failed
                )
            });
        if let Some(task) = reusable_task {
            if task.state == TaskState::Failed {
                runtime.tasks.transition_task(&task.id, TaskState::Queued)?;
            }
            runtime
                .tasks
                .transition_task(&task.id, TaskState::Running)?;
            task.id
        } else {
            let retry_policy = RetryPolicy {
                max_attempts: runtime.config.agent.retry_max_attempts,
                backoff_secs: runtime.config.agent.retry_backoff_secs,
                max_backoff_secs: runtime.config.agent.retry_max_backoff_secs,
            };
            let mut task =
                TaskRecord::new_with_retry_policy(GoalSpec::new(goal_text), retry_policy)
                    .map_err(anyhow::Error::msg)?;
            task.session_id = Some(session_id.clone());
            let task_id = task.id.clone();
            runtime.tasks.create_task(task)?;
            runtime
                .tasks
                .transition_task(&task_id, TaskState::Running)?;
            task_id
        }
    };
    let attempt = runtime
        .tasks
        .latest_run_for_task(&task_id)
        .map(|run| run.attempt.saturating_add(1))
        .unwrap_or(1);
    let run = runtime.tasks.create_run(&task_id, attempt)?;
    runtime.register_agent_handle(task_id.clone(), control_handle);
    let _active_agent_guard = ActiveAgentGuard {
        runtime: runtime.clone(),
        task_id: task_id.clone(),
    };
    let lease = runtime.workers.acquire(&run.id)?;
    let run_id = lease.run_id().to_string();
    {
        let mut s = session.write();
        s.append(SessionEvent::TurnStart {
            id: turn_id.clone(),
            at: Utc::now(),
            profile_id: model_identity.profile_id.clone(),
            model_id: model_identity.model_id.clone(),
            family: model_identity.family.clone(),
        });
        s.append(SessionEvent::UserMessage {
            id: Uuid::new_v4().to_string(),
            text: user_text.clone(),
            at: Utc::now(),
        });
    }
    record_event(
        &runtime,
        EventEnvelope::new(
            "turn.started",
            json!({"session_id": session_id, "user_text": user_text.clone(),
                "profile_id": model_identity.profile_id, "model_id": model_identity.model_id,
                "family": model_identity.family}),
        )
        .with_source(EventSource::Core)
        .with_turn(turn_id.clone()),
        Some(&task_id),
        Some(&run_id),
    );
    record_event(
        &runtime,
        EventEnvelope::new("user.message", json!({"text": user_text}))
            .with_source(EventSource::User)
            .with_turn(turn_id.clone()),
        Some(&task_id),
        Some(&run_id),
    );
    let _ = event_tx
        .send(AgentEvent::TurnStarted(turn_id.clone()))
        .await;

    {
        let bypass = runtime.settings.read().bypass_hook_trust;
        let notes = runtime
            .hooks
            .read()
            .run_event("turn_start", &runtime.workspace_root, bypass);
        for n in notes {
            session.write().append(SessionEvent::SystemNote {
                id: Uuid::new_v4().to_string(),
                text: n,
                at: Utc::now(),
            });
        }
    }

    // Refresh model-size guidance after a `/model` or CLI override. The policy
    // is immutable for this turn, which keeps all steps internally consistent
    // even if another frontend changes the active model concurrently.
    let model_profile =
        crate::ModelProfile::for_model(turn_llm.config().model, &runtime.config.llm.optimization);
    turn_prompt.set_model_optimization(model_profile.prompt_section());
    let mut model_policy = model_profile.policy.clone();
    let base_llm_config = turn_llm.config();
    if model_profile.small_model {
        if let Some(max_tokens) = model_policy.max_tokens {
            let configured_max = if base_llm_config.max_tokens == 0 {
                256
            } else {
                base_llm_config.max_tokens
            };
            model_policy.max_tokens = Some(max_tokens.min(configured_max).max(1));
        }
        if let Some(max_chars) = model_policy.tool_result_max_chars {
            let configured_max = if runtime.config.agent.tool_result_max_chars == 0 {
                512
            } else {
                runtime.config.agent.tool_result_max_chars
            };
            model_policy.tool_result_max_chars = Some(max_chars.min(configured_max).max(1));
        }
    }
    // Adaptive values are defaults. Explicit ARIS settings must remain effective
    // for small models as well. Read here, not in model_profile(), which is also
    // called while update_settings owns the settings write lock.
    {
        let settings = runtime.settings.read();
        if settings.temperature.is_some() {
            model_policy.temperature = Some(base_llm_config.temperature);
        }
        if settings.thinking.is_some() {
            model_policy.thinking = Some(base_llm_config.thinking);
        }
        if settings.max_tokens.is_some() {
            model_policy.max_tokens = Some(base_llm_config.max_tokens);
        }
    }

    // agent/pre-step analogue: one-pass cognition before first model request.
    let thought_notes = prepare_turn_cognition(
        &runtime,
        &mut turn_prompt,
        &user_text,
        model_profile.small_model,
    );
    // Non-blocking UI ticks — do not delay llm/stream TTFB.
    for (t, note) in thought_notes {
        let _ = event_tx.try_send(AgentEvent::ThoughtTick { t, note });
    }

    let task_goal = runtime.tasks.task(&task_id).map(|task| task.goal);
    let max_steps = task_goal
        .as_ref()
        .and_then(|goal| goal.max_steps)
        .map(|steps| steps.min(usize::MAX as u64) as usize)
        .unwrap_or(runtime.config.agent.max_steps_per_turn);
    let max_steps = model_policy
        .max_steps
        .map(|limit| max_steps.min(limit))
        .unwrap_or(max_steps)
        .min(execution_limits.map_or(10_000, |limits| limits.0))
        .min(10_000);
    let deadline = task_goal.as_ref().and_then(|goal| goal.deadline);
    let mut completed_response = false;
    // Cache tool schemas across steps until registry generation changes.
    let mut tool_gen = u64::MAX;
    let mut tool_specs: Arc<[ToolSpec]> = Arc::from([]);
    let owner = runtime.tasks.task(&task_id).and_then(|task| task.owner);
    let team_worker = owner.is_some();
    let worker_brief = if let Some(owner) = owner {
        let team = runtime.teams.get(&owner.team_id)?;
        let worker = team
            .agents
            .iter()
            .find(|agent| agent.id == owner.agent_id)
            .context("Worker assignment missing")?;
        let instruction = team
            .messages
            .iter()
            .rev()
            .find(|msg| !msg.queued && msg.agent_id.as_deref() == Some(worker.id.as_str()));
        let peers: Vec<_> = team
            .agents
            .iter()
            .filter(|a| a.id != worker.id && a.subagent_id.is_some())
            .map(|a| a.id.as_str())
            .collect();
        Some(json!({"role":worker.id,"assignment":worker.prompt,"latest_instruction":instruction.map(|msg| &msg.text),"write_to":worker.files,"peer_ids":peers}).to_string())
    } else {
        None
    };
    let output_check = crate::synthesis::delivery_check(&runtime, &task_id)?;
    let mut requested_correction = false;
    let mut missing_files = Vec::<String>::new();
    let mut check_written_files = false;
    let mut documents_ready = false;
    let context_chars = if execution_limits.is_some() {
        Some(model_policy.context_chars.unwrap_or(64_000).min(64_000))
    } else {
        model_policy.context_chars
    };
    let context_messages = if execution_limits.is_some() {
        Some(model_policy.context_messages.unwrap_or(32).min(32))
    } else {
        model_policy.context_messages
    };
    let system_limit = context_chars.map(|chars| (chars / 3).max(2_048));
    let message_limit =
        context_chars.map(|chars| chars.saturating_sub(system_limit.unwrap_or(0)).max(2_048));
    let system = turn_prompt.render_with_limit(system_limit);
    let mut tool_calls_used = 0usize;

    for _step in 0..max_steps {
        if *cancel_rx.borrow() || *pause_rx.borrow() {
            break;
        }
        if deadline.is_some_and(|at| Utc::now() >= at) {
            record_event(
                &runtime,
                EventEnvelope::new("task.deadline_exceeded", json!({"deadline": deadline}))
                    .with_source(EventSource::System)
                    .with_turn(turn_id.clone()),
                Some(&task_id),
                Some(&run_id),
            );
            record_task_feedback(&runtime, &task_id, &run_id, false, "goal deadline exceeded");
            finish_durable_run(&lease, TaskState::Failed, RunState::Failed);
            return Err(anyhow::anyhow!("goal deadline exceeded"));
        }

        let step_id = Uuid::new_v4().to_string();
        session.write().append(SessionEvent::StepStart {
            id: step_id.clone(),
            turn_id: turn_id.clone(),
            at: Utc::now(),
        });
        record_event(
            &runtime,
            EventEnvelope::new("step.started", json!({"step_index": _step + 1}))
                .with_source(EventSource::Core)
                .with_turn(turn_id.clone())
                .with_step(step_id.clone()),
            Some(&task_id),
            Some(&run_id),
        );

        runtime.computer_status();
        let peer_context = crate::tunnel::context(&runtime, &task_id)?;
        if check_written_files {
            if let Some(check) = &output_check {
                documents_ready = check.missing(&session.read())?.is_empty();
            }
            check_written_files = false;
        }
        let conversation_limit = message_limit.map(|limit| {
            limit
                .saturating_sub(
                    peer_context
                        .as_ref()
                        .map_or(0, |context| context.chars().count()),
                )
                .max(2_048)
        });
        let mut messages = session.read().derive_messages_with_budget(
            &system,
            context_messages,
            conversation_limit,
            model_policy.thinking.unwrap_or(base_llm_config.thinking),
        );
        // Crawled pages and long tool batches can evict the current user turn
        // from the bounded history. Keep the actual commission (including its
        // source deliveries) in front of the surviving tool conversation.
        if team_worker
            && !messages
                .iter()
                .any(|message| message.role == aris_llm::Role::User && message.text() == user_text)
        {
            messages.insert(1, aris_llm::ChatMessage::user(user_text.clone()));
        }
        if let Some(context) = peer_context {
            messages.push(aris_llm::ChatMessage::user(context));
        }
        if let Some(brief) = &worker_brief {
            if _step > 0 {
                let next_action = if documents_ready {
                    "All assigned documents have verified content for this run. Address any outstanding peer request, then return your final handoff with the file paths and findings. Reopen sources or revise files only to correct a specific outstanding issue."
                } else {
                    "Use the tools to complete your assigned files and to send any requested questions, proposals or replies to the named peers."
                };
                let mut reminder = format!("Continue your current ARIS assignment using the observations above. Answer in the user's language. Your recipient already supplied the brief. {next_action} Return concrete findings and sources.\n{brief}");
                if let Some((_, _, limit)) = execution_limits {
                    let steps_left = max_steps - _step;
                    let calls_left = limit.saturating_sub(tool_calls_used);
                    if steps_left <= 4 || calls_left <= 6 {
                        reminder.push_str(&format!("\nRemaining budget including this step: {steps_left} model responses and {calls_left} tool calls. Deliver from the evidence already collected. Reserve time for the outstanding peer reply, writing the declared files, and a final answer. Disclose any genuine evidence gap in the report."));
                    }
                }
                if !missing_files.is_empty() {
                    reminder.push_str(&format!("\nThese assigned documents still need a verified write. Use write_file to finish them now: {}",missing_files.join(", ")));
                    missing_files.clear();
                }
                messages.push(aris_llm::ChatMessage::user(reminder));
            }
        }
        if vision {
            session
                .read()
                .append_computer_observation(&mut messages, &runtime.outer_home);
        }
        let gen = runtime.tools.generation();
        if gen != tool_gen {
            tool_gen = gen;
            let definitions: Vec<_> = runtime
                .tools
                .definitions()
                .into_iter()
                .filter(|definition| team_worker || definition.name != "agent_tunnel")
                .collect();
            let worker_tools: Vec<_> = definitions
                .iter()
                .filter(|definition| {
                    team_worker
                        && (definition.name == "agent_tunnel"
                            || (output_check.is_some() && definition.name == "write_file"))
                })
                .cloned()
                .collect();
            let mut definitions =
                select_tools_for_query(&definitions, &user_text, model_policy.tool_budget);
            for required in worker_tools {
                if !definitions
                    .iter()
                    .any(|definition| definition.name == required.name)
                {
                    definitions.push(required);
                }
            }
            tool_specs = definitions
                .into_iter()
                .map(|d| ToolSpec {
                    kind: "function".into(),
                    function: ToolFunctionSpec {
                        name: d.name,
                        description: if model_policy.compact_tool_schemas {
                            compact_tool_description(&d.description, 320)
                        } else {
                            d.description
                        },
                        parameters: if model_policy.compact_tool_schemas {
                            compact_tool_schema(
                                &d.parameters,
                                model_policy.tool_schema_chars.unwrap_or(2_400),
                            )
                        } else {
                            d.parameters
                        },
                    },
                })
                .collect();
        }

        let (llm_tx, mut llm_rx) = mpsc::channel(256);
        let llm = turn_llm.clone();
        // An additional tool call on the final permitted step would have no
        // model response to consume its result. Close with the real documents
        // once their writes have been verified, including any open questions.
        let tools_for_llm = if documents_ready && _step + 1 == max_steps {
            Arc::from([])
        } else {
            tool_specs.clone()
        };
        let mut cancel_for_llm = cancel_rx.clone();
        let request_options = if model_policy.max_tokens.is_some()
            || model_policy.temperature.is_some()
            || model_policy.thinking.is_some()
            || model_policy.parallel_tool_calls.is_some()
        {
            Some(LlmRequestOptions {
                thinking: model_policy.thinking,
                max_tokens: model_policy.max_tokens,
                temperature: model_policy.temperature,
                parallel_tool_calls: model_policy.parallel_tool_calls,
                extra_body: None,
            })
        } else {
            None
        };
        let llm_task = tokio::spawn(async move {
            let stream_cancel = cancel_for_llm.clone();
            let request = llm.stream_chat_cancellable_with_options(
                &messages,
                &tools_for_llm,
                llm_tx,
                Some(stream_cancel),
                request_options,
            );
            // Some gateways do not send response headers until generation has
            // finished. Cancellation must also interrupt that first-byte wait.
            tokio::select! {
                result = request => result,
                _ = async {
                    loop {
                        if *cancel_for_llm.borrow() { break; }
                        if cancel_for_llm.changed().await.is_err() { break; }
                    }
                } => Err(aris_llm::LlmError::Api("cancelled".into())),
            }
        });

        // Stream path: forward to UI immediately; do NOT append per-token chunk
        // events (official keeps chunks for UI fidelity via live events; durable
        // assistant/message is written once after assemble — preserves token rate).
        while let Some(ev) = llm_rx.recv().await {
            match ev {
                LlmEvent::TextDelta(t) => {
                    let _ = event_tx.try_send(AgentEvent::TextDelta(t));
                }
                LlmEvent::ReasoningDelta(t) => {
                    let _ = event_tx.try_send(AgentEvent::ReasoningDelta(t));
                }
                LlmEvent::Error(e) => {
                    let _ = event_tx.send(AgentEvent::Error(e)).await;
                }
                _ => {}
            }
        }

        let assembled = match llm_task.await? {
            Ok(a) => a,
            Err(e) if e.to_string().contains("cancelled") => {
                session.write().append(SessionEvent::SystemNote {
                    id: Uuid::new_v4().to_string(),
                    text: "turn cancelled during LLM stream".into(),
                    at: Utc::now(),
                });
                runtime.sessions.persist_now(&session);
                record_event(
                    &runtime,
                    EventEnvelope::new("turn.cancelled", json!({"reason": "llm_cancelled"}))
                        .with_source(EventSource::System)
                        .with_turn(turn_id.clone()),
                    Some(&task_id),
                    Some(&run_id),
                );
                break;
            }
            Err(e) => {
                record_event(
                    &runtime,
                    EventEnvelope::new("turn.failed", json!({"error": e.to_string()}))
                        .with_source(EventSource::System)
                        .with_turn(turn_id.clone()),
                    Some(&task_id),
                    Some(&run_id),
                );
                record_task_feedback(&runtime, &task_id, &run_id, false, &e.to_string());
                finish_durable_run(&lease, TaskState::Failed, RunState::Failed);
                return Err(e.into());
            }
        };
        session.write().append(SessionEvent::AssistantMessage {
            provider_items: assembled.provider_items.clone(),
            id: Uuid::new_v4().to_string(),
            text: assembled.text.clone(),
            reasoning: assembled.reasoning.clone(),
            at: Utc::now(),
        });
        record_event(
            &runtime,
            EventEnvelope::new(
                "assistant.message",
                json!({"text": assembled.text.clone(), "reasoning": assembled.reasoning.clone()}),
            )
            .with_source(EventSource::Model)
            .with_turn(turn_id.clone())
            .with_step(step_id.clone()),
            Some(&task_id),
            Some(&run_id),
        );

        let tool_calls = match assembled.parsed_tool_calls() {
            Ok(tool_calls) => tool_calls,
            Err(err) => {
                record_task_feedback(&runtime, &task_id, &run_id, false, &err.to_string());
                finish_durable_run(&lease, TaskState::Failed, RunState::Failed);
                return Err(err);
            }
        };
        let needs_tools =
            !tool_calls.is_empty() && assembled.finish_reason == FinishReason::ToolCalls;
        if !needs_tools {
            let peer_arrived = _step + 1 < max_steps
                && crate::tunnel::has_unread_request_or_result(&runtime, &task_id)?;
            missing_files = match &output_check {
                Some(check) => check.missing(&session.read())?,
                None => vec![],
            };
            let repair =
                !missing_files.is_empty() && !requested_correction && _step + 1 < max_steps;
            if repair {
                requested_correction = true;
                session.write().append(SessionEvent::SystemNote {
                    id: Uuid::new_v4().to_string(),
                    text: format!("待补齐交付文件：{}", missing_files.join("、")),
                    at: Utc::now(),
                });
            }
            completed_response = !peer_arrived && !repair;
            session.write().append(SessionEvent::StepEnd {
                id: step_id.clone(),
                turn_id: turn_id.clone(),
                at: Utc::now(),
            });
            record_event(
                &runtime,
                EventEnvelope::new("step.ended", json!({"reason": "stop"}))
                    .with_source(EventSource::Core)
                    .with_turn(turn_id.clone())
                    .with_step(step_id.clone()),
                Some(&task_id),
                Some(&run_id),
            );
            save_step_checkpoint(&runtime, &session, &task_id, &run_id, _step);
            if peer_arrived || repair {
                continue;
            }
            if !missing_files.is_empty() {
                let reason = format!(
                    "Assigned delivery is incomplete: {}",
                    missing_files.join(", ")
                );
                record_task_feedback(&runtime, &task_id, &run_id, false, &reason);
                finish_durable_run(&lease, TaskState::Failed, RunState::Failed);
                anyhow::bail!(reason);
            }
            break;
        }

        let tool_ctx = ToolContext {
            agent_id: session_id.clone(),
            cwd: runtime.workspace_root.clone(),
            outer_home: runtime.outer_home.clone(),
            workspace_outer: runtime.workspace_outer.clone(),
            cancel: cancel_rx.clone(),
        };

        for (call_id, name, args) in tool_calls {
            tool_calls_used += 1;
            if execution_limits.is_some_and(|limits| tool_calls_used > limits.2) {
                let message =
                    "Admin tool-call budget exhausted; send a new instruction to continue";
                runtime.sessions.persist_now(&session);
                record_task_feedback(&runtime, &task_id, &run_id, false, message);
                finish_durable_run(&lease, TaskState::Failed, RunState::Failed);
                return Err(anyhow::anyhow!(message));
            }
            if *cancel_rx.borrow() || *pause_rx.borrow() {
                break;
            }
            if deadline.is_some_and(|at| Utc::now() >= at) {
                record_task_feedback(&runtime, &task_id, &run_id, false, "goal deadline exceeded");
                finish_durable_run(&lease, TaskState::Failed, RunState::Failed);
                return Err(anyhow::anyhow!("goal deadline exceeded"));
            }
            session.write().append(SessionEvent::ToolCall {
                id: Uuid::new_v4().to_string(),
                call_id: call_id.clone(),
                name: name.clone(),
                arguments: args.clone(),
                at: Utc::now(),
            });
            record_event(
                &runtime,
                EventEnvelope::new(
                    "tool.called",
                    json!({"call_id": call_id.clone(), "name": name.clone(), "arguments": args.clone()}),
                )
                .with_source(EventSource::Model)
                .with_turn(turn_id.clone())
                .with_step(step_id.clone()),
            Some(&task_id),
            Some(&run_id),
        );
            let _ = event_tx
                .send(AgentEvent::ToolStarted {
                    name: name.clone(),
                    call_id: call_id.clone(),
                })
                .await;

            let call = ToolCall {
                id: call_id.clone(),
                name: name.clone(),
                arguments: args.clone(),
            };
            let perm = *runtime.permissions.read();
            let approval = runtime.settings.read().approval;
            let tool_result = match runtime.tools.preflight(&call) {
                Err(err) => aris_tools::ToolResult::error(err.to_string()),
                Ok(definition) => {
                    let metadata_aware = !definition.metadata.capabilities.is_empty();
                    let allowed = if metadata_aware {
                        perm.allows_metadata(&definition.metadata)
                    } else {
                        perm.allows_tool(&name)
                    };
                    let needs_approval = if metadata_aware {
                        approval.requires_metadata(&definition.metadata)
                    } else {
                        approval.requires_approval(&name)
                    };
                    if !allowed {
                        let reason = perm.deny_reason(&name);
                        runtime
                            .approvals
                            .record_denial(crate::approvals::DeniedAction {
                                call_id: call_id.clone(),
                                name: name.clone(),
                                arguments: args.clone(),
                                reason: reason.clone(),
                            });
                        aris_tools::ToolResult::error(reason)
                    } else if needs_approval {
                        transition_durable_state(
                            &runtime,
                            &task_id,
                            &run_id,
                            TaskState::WaitingApproval,
                            RunState::WaitingApproval,
                        );
                        let summary: String = args.to_string().chars().take(240).collect();
                        let allowed = runtime
                            .request_tool_approval_scoped(
                                &event_tx,
                                &call_id,
                                &name,
                                &summary,
                                ApprovalScope::new(Some(&task_id), Some(&run_id)),
                                &mut cancel_rx,
                            )
                            .await;
                        transition_durable_state(
                            &runtime,
                            &task_id,
                            &run_id,
                            TaskState::Running,
                            RunState::Running,
                        );
                        if !allowed {
                            let reason = format!(
                                "tool `{name}` denied by user (approval policy {})",
                                approval.label()
                            );
                            runtime
                                .approvals
                                .record_denial(crate::approvals::DeniedAction {
                                    call_id: call_id.clone(),
                                    name: name.clone(),
                                    arguments: args.clone(),
                                    reason: reason.clone(),
                                });
                            aris_tools::ToolResult::error(reason)
                        } else {
                            execute_tool_with_timeout(
                                &runtime,
                                &call,
                                &tool_ctx,
                                definition.metadata.timeout_secs,
                            )
                            .await
                        }
                    } else {
                        execute_tool_with_timeout(
                            &runtime,
                            &call,
                            &tool_ctx,
                            definition.metadata.timeout_secs,
                        )
                        .await
                    }
                }
            };

            let mut content = tool_result.content.clone();
            let max = model_policy
                .tool_result_max_chars
                .unwrap_or(runtime.config.agent.tool_result_max_chars);
            if content.chars().count() > max && name.starts_with("computer_") {
                if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&content) {
                    let screenshot = value.get("screenshot").cloned();
                    let text = value
                        .get("text")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("");
                    value = json!({"text": truncate_chars(text, max.saturating_sub(1024)), "screenshot": screenshot, "truncated":true});
                    content = value.to_string();
                }
            } else if name != "agent_tunnel" && content.chars().count() > max {
                content = if model_policy.compact_tool_results {
                    compact_tool_result(&content, max)
                } else {
                    truncate_chars(&content, max)
                };
            }

            session.write().append(SessionEvent::ToolResult {
                id: Uuid::new_v4().to_string(),
                call_id: call_id.clone(),
                name: name.clone(),
                ok: tool_result.ok,
                content: content.clone(),
                at: Utc::now(),
            });
            if output_check.is_some()
                && tool_result.ok
                && matches!(
                    name.as_str(),
                    "write_file" | "edit_file" | "create_file" | "apply_patch" | "shell"
                )
            {
                check_written_files = true;
            }
            record_event(
                &runtime,
                EventEnvelope::new(
                    "tool.result",
                    json!({"call_id": call_id.clone(), "name": name.clone(), "ok": tool_result.ok, "content": content.clone()}),
                )
                .with_source(EventSource::Tool)
                .with_turn(turn_id.clone())
                .with_step(step_id.clone()),
            Some(&task_id),
            Some(&run_id),
        );

            if runtime.config.learn.enabled && runtime.settings.read().memory_generate {
                let note = if name == "skill_load" {
                    format!(
                        "loaded skill {}",
                        args.get("name").and_then(|v| v.as_str()).unwrap_or("?")
                    )
                } else {
                    content.chars().take(100).collect::<String>()
                };
                runtime.learn.record_tool_outcome_detailed(
                    &user_text,
                    &name,
                    args.get("name").and_then(|v| v.as_str()),
                    tool_result.ok,
                    note,
                );
            }
            if name == "skill_load" {
                if let Some(sk) = args.get("name").and_then(|v| v.as_str()) {
                    runtime.ctm.observe_skill_load(sk, tool_result.ok);
                }
            } else {
                runtime.ctm.observe_tool(&name, tool_result.ok);
            }

            let preview: String = content.chars().take(160).collect();
            let _ = event_tx
                .send(AgentEvent::ToolFinished {
                    name,
                    call_id,
                    ok: tool_result.ok,
                    preview,
                })
                .await;
        }

        session.write().append(SessionEvent::StepEnd {
            id: step_id.clone(),
            turn_id: turn_id.clone(),
            at: Utc::now(),
        });
        record_event(
            &runtime,
            EventEnvelope::new("step.ended", json!({"reason": "tool_calls"}))
                .with_source(EventSource::Core)
                .with_turn(turn_id.clone())
                .with_step(step_id.clone()),
            Some(&task_id),
            Some(&run_id),
        );
        // Debounced durable flush between steps (not per tool / not per token).
        runtime.sessions.mark_dirty(&session);
        runtime.sessions.flush_dirty();
        runtime.learn.flush_dirty();
        save_step_checkpoint(&runtime, &session, &task_id, &run_id, _step);
    }

    if !completed_response && !*cancel_rx.borrow() && !*pause_rx.borrow() {
        let message = if max_steps == 0 {
            "goal has no executable steps"
        } else {
            "maximum agent steps exceeded"
        };
        record_event(
            &runtime,
            EventEnvelope::new("task.max_steps_exceeded", json!({"max_steps": max_steps}))
                .with_source(EventSource::System)
                .with_turn(turn_id.clone()),
            Some(&task_id),
            Some(&run_id),
        );
        record_task_feedback(&runtime, &task_id, &run_id, false, message);
        finish_durable_run(&lease, TaskState::Failed, RunState::Failed);
        return Err(anyhow::anyhow!(message));
    }

    session.write().append(SessionEvent::TurnEnd {
        id: turn_id.clone(),
        at: Utc::now(),
    });
    runtime.sessions.persist_now(&session);
    runtime.learn.flush_dirty();
    if *cancel_rx.borrow() {
        record_task_feedback(&runtime, &task_id, &run_id, false, "run cancelled");
        finish_durable_run(&lease, TaskState::Cancelled, RunState::Cancelled);
    } else if *pause_rx.borrow() {
        record_task_feedback(&runtime, &task_id, &run_id, false, "run paused");
        match runtime.tasks.task(&task_id).map(|task| task.state) {
            Some(TaskState::Queued) => {
                finish_durable_run(&lease, TaskState::Queued, RunState::Cancelled)
            }
            Some(TaskState::Cancelled) => {
                finish_durable_run(&lease, TaskState::Cancelled, RunState::Cancelled)
            }
            Some(TaskState::Completed) => {
                finish_durable_run(&lease, TaskState::Completed, RunState::Completed)
            }
            _ => finish_durable_run(&lease, TaskState::Paused, RunState::Paused),
        }
    } else {
        let goal = runtime.tasks.task(&task_id).map(|task| task.goal);
        let report = goal.as_ref().map(|goal| {
            if goal.has_verification() {
                record_event(
                    &runtime,
                    EventEnvelope::new(
                        "verification.started",
                        json!({"criteria": goal.verification}),
                    )
                    .with_source(EventSource::Core)
                    .with_turn(turn_id.clone()),
                    Some(&task_id),
                    Some(&run_id),
                );
                verify_goal(&runtime.workspace_root, goal, &session.read())
            } else {
                record_event(
                    &runtime,
                    EventEnvelope::new("verification.skipped", json!({"reason": "no_criteria"}))
                        .with_source(EventSource::Core)
                        .with_turn(turn_id.clone()),
                    Some(&task_id),
                    Some(&run_id),
                );
                crate::verification::VerificationReport::skipped()
            }
        });
        let verification_passed = report.as_ref().is_none_or(|report| report.passed);
        if let Some(report) = report.as_ref().filter(|report| !report.checks.is_empty()) {
            let event_type = if report.passed {
                "verification.passed"
            } else {
                "verification.failed"
            };
            record_event(
                &runtime,
                EventEnvelope::new(event_type, json!(report))
                    .with_source(if report.passed {
                        EventSource::Core
                    } else {
                        EventSource::System
                    })
                    .with_turn(turn_id.clone()),
                Some(&task_id),
                Some(&run_id),
            );
        }
        if verification_passed {
            record_task_feedback(&runtime, &task_id, &run_id, true, "run completed");
            finish_durable_run(&lease, TaskState::Completed, RunState::Completed);
        } else {
            let detail = report
                .as_ref()
                .and_then(|report| report.checks.iter().find(|check| !check.passed))
                .map(|check| format!("{}: {}", check.criterion, check.detail))
                .unwrap_or_else(|| "goal verification failed".into());
            record_task_feedback(&runtime, &task_id, &run_id, false, &detail);
            finish_durable_run(&lease, TaskState::Failed, RunState::Failed);
            return Err(anyhow::anyhow!(detail));
        }
    }
    record_event(
        &runtime,
        EventEnvelope::new("turn.ended", json!({"session_id": session_id}))
            .with_source(EventSource::Core)
            .with_turn(turn_id.clone()),
        Some(&task_id),
        Some(&run_id),
    );
    let _ = event_tx.send(AgentEvent::TurnEnded(turn_id)).await;
    Ok(())
}

/// Official-aligned `agent/pre-step`: assemble routing sections once.
/// Returns CTM tick notes for non-blocking UI fan-out.
fn prepare_turn_cognition(
    runtime: &Runtime,
    prompt: &mut crate::SystemPromptBuilder,
    user_text: &str,
    small_model: bool,
) -> Vec<(usize, String)> {
    let settings = runtime.settings.read().clone();
    let memory_inject = runtime.config.learn.enabled && settings.memory_inject;
    runtime.learn.set_enabled(
        memory_inject,
        runtime.config.learn.enabled && settings.memory_generate,
    );
    let skill_topk = if small_model {
        runtime.config.agent.skill_prompt_topk.min(2)
    } else {
        runtime.config.agent.skill_prompt_topk
    };
    let plugin_topk = if small_model {
        runtime.config.agent.plugin_prompt_topk.min(1)
    } else {
        runtime.config.agent.plugin_prompt_topk
    };
    let weights = runtime.learn.weights();
    let mut boost = weights;

    // Cheap channel registration (names only) — no BM25 before CTM.
    let skill_names: Vec<String> = runtime
        .skills
        .read()
        .as_ref()
        .map(|c| c.list().into_iter().map(|s| s.name).take(64).collect())
        .unwrap_or_default();
    let plugin_ids: Vec<String> = runtime
        .plugins
        .read()
        .as_ref()
        .map(|p| p.ids())
        .unwrap_or_default();

    runtime.ctm.ensure_channels(&skill_names, &plugin_ids);
    let snapshot = runtime.ctm.think(user_text, &skill_names, &plugin_ids);

    for (i, name) in snapshot.recommended_skills.iter().enumerate() {
        let key = format!("skill:{name}");
        let bump = 1.8 - i as f32 * 0.15;
        *boost.entry(key).or_insert(0.0) += bump.max(0.4);
    }
    for (i, id) in snapshot.recommended_plugins.iter().enumerate() {
        let key = format!("plugin:{id}");
        let bump = 1.6 - i as f32 * 0.15;
        *boost.entry(key).or_insert(0.0) += bump.max(0.3);
    }

    let mut sections: HashMap<&str, String> = HashMap::new();
    sections.insert("learn", runtime.learn.prompt_section(user_text));

    // Single-pass rank after CTM boosts (was double-ranked).
    if let Some(skills) = runtime.skills.read().clone() {
        let ranked = aris_skill::rank_skills(&skills, user_text, &boost, skill_topk);
        sections.insert("skills", aris_skill::prompt_topk_section(&ranked));
    }
    if let Some(plugins) = runtime.plugins.read().clone() {
        let ranked = aris_plugin::rank_plugins(&plugins, user_text, &boost, plugin_topk);
        sections.insert("plugins", aris_plugin::prompt_topk_section(&ranked));
    }
    if memory_inject && !snapshot.prompt_section.is_empty() {
        sections.insert("continuous_thought", snapshot.prompt_section);
    } else if !memory_inject {
        sections.insert("continuous_thought", String::new());
    }

    prompt.set_sections(sections);

    snapshot.ticks.into_iter().map(|t| (t.t, t.note)).collect()
}

fn truncate_chars(s: &str, max: usize) -> String {
    let mut out: String = s.chars().take(max).collect();
    out.push_str("\n…[truncated]");
    out
}

fn record_event(
    runtime: &Runtime,
    mut event: EventEnvelope,
    task_id: Option<&str>,
    run_id: Option<&str>,
) {
    if let Some(task_id) = task_id {
        event = event.with_task(task_id.to_string());
    }
    if let Some(run_id) = run_id {
        event = event.with_run(run_id.to_string());
    }
    if let Err(err) = runtime.record_event(event) {
        tracing::error!(error = %err, "failed to persist protocol event");
    }
}

fn save_step_checkpoint(
    runtime: &Runtime,
    session: &Arc<RwLock<Session>>,
    task_id: &str,
    run_id: &str,
    step: usize,
) {
    let mut checkpoint = CheckpointRecord::new(
        task_id.to_string(),
        run_id.to_string(),
        runtime.events.latest_sequence(),
    );
    checkpoint.step_index = (step + 1) as u32;
    checkpoint.session_event_count = session.read().events.len() as u64;
    if let Err(err) = runtime.tasks.save_checkpoint(checkpoint) {
        tracing::warn!(error = %err, task_id, run_id, step = step + 1, "failed to persist step checkpoint");
    }
}

fn finish_durable_run(lease: &RunLease, task_state: TaskState, run_state: RunState) {
    if let Err(err) = lease.finish(task_state, run_state) {
        tracing::warn!(
            error = %err,
            task_id = lease.task_id(),
            run_id = lease.run_id(),
            "failed to finish durable run"
        );
    }
}

struct ActiveAgentGuard {
    runtime: Arc<Runtime>,
    task_id: String,
}

impl Drop for ActiveAgentGuard {
    fn drop(&mut self) {
        self.runtime.clear_agent_handle(&self.task_id);
    }
}

fn record_task_feedback(runtime: &Runtime, task_id: &str, run_id: &str, ok: bool, note: &str) {
    if !runtime.config.learn.enabled || !runtime.settings.read().memory_generate {
        return;
    }
    let Some(task) = runtime.tasks.task(task_id) else {
        return;
    };
    let attempt = runtime
        .tasks
        .run(run_id)
        .map(|run| run.attempt)
        .unwrap_or(1);
    runtime
        .learn
        .record_task_outcome(task_id, run_id, &task.goal.outcome, attempt, ok, note);
    let _ = runtime.record_event(
        EventEnvelope::new(
            "learning.feedback",
            json!({
                "task_id": task_id,
                "run_id": run_id,
                "attempt": attempt,
                "goal": task.goal.outcome,
                "ok": ok,
                "note": note,
            }),
        )
        .with_source(EventSource::System)
        .with_task(task_id.to_string())
        .with_run(run_id.to_string()),
    );
    runtime.learn.flush_dirty();
}

fn transition_durable_state(
    runtime: &Runtime,
    task_id: &str,
    run_id: &str,
    task_state: TaskState,
    run_state: RunState,
) {
    if let Err(err) = runtime.tasks.transition_task(task_id, task_state) {
        tracing::warn!(error = %err, task_id, "failed to transition durable task state");
    }
    if let Err(err) = runtime.tasks.transition_run(run_id, run_state) {
        tracing::warn!(error = %err, run_id, "failed to transition durable run state");
    }
}

async fn execute_tool_with_timeout(
    runtime: &Runtime,
    call: &ToolCall,
    ctx: &ToolContext,
    metadata_timeout_secs: Option<u64>,
) -> aris_tools::ToolResult {
    let timeout_secs = metadata_timeout_secs
        .unwrap_or(runtime.config.agent.tool_timeout_secs)
        .max(1);
    match tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        runtime.pipeline.execute(call, ctx),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => aris_tools::ToolResult::error("tool timed out"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cognition_drops_stale_memory_when_existing_tui_toggle_changes() {
        let fixture = crate::tests::RuntimeFixture::new(crate::SessionSettings::default(), true);
        let runtime = &fixture.runtime;
        runtime
            .learn
            .record_tool_outcome("fixture task", "shell", true, "private learned note");
        let mut prompt = runtime.prompt.read().clone();
        prepare_turn_cognition(runtime, &mut prompt, "fixture task", false);
        assert!(prompt.render().contains("private learned note"));
        runtime.settings.write().memory_inject = false;
        prepare_turn_cognition(runtime, &mut prompt, "fixture task", false);
        assert!(!prompt.render().contains("private learned note"));
        assert!(!runtime
            .prompt
            .read()
            .render()
            .contains("private learned note"));
        assert!(runtime.learn.weights().is_empty());
        assert!(runtime.learn.recall("fixture task", 5).is_empty());
    }

    #[test]
    fn agent_event_protocol_projection_keeps_correlations() {
        let mut context = AgentEventContext::for_session("session-1");
        context.task_id = Some("task-1".into());
        context.run_id = Some("run-1".into());
        context.turn_id = Some("turn-1".into());
        context.step_id = Some("step-1".into());
        let event = AgentEvent::ToolFinished {
            name: "read_file".into(),
            call_id: "call-1".into(),
            ok: true,
            preview: "ok".into(),
        }
        .to_protocol_event(&context);
        assert_eq!(event.schema_version, aris_protocol::SCHEMA_VERSION);
        assert_eq!(event.sequence, 0);
        assert_eq!(event.task_id.as_deref(), Some("task-1"));
        assert_eq!(event.run_id.as_deref(), Some("run-1"));
        assert_eq!(event.turn_id.as_deref(), Some("turn-1"));
        assert_eq!(event.step_id.as_deref(), Some("step-1"));
        assert_eq!(event.payload["session_id"], "session-1");
    }

    #[tokio::test]
    async fn parallel_profiles_keep_models_and_roles_frozen_across_tool_steps() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (ready_tx, mut ready_rx) = mpsc::channel(2);
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let server_release = release.clone();
        let observed = Arc::new(parking_lot::Mutex::new(Vec::<serde_json::Value>::new()));
        let server_observed = observed.clone();
        let server = tokio::spawn(async move {
            let mut workers = Vec::new();
            for _ in 0..5 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let ready = ready_tx.clone();
                let release = server_release.clone();
                let observed = server_observed.clone();
                workers.push(tokio::spawn(async move {
                    let mut request = Vec::new(); let mut buffer = [0u8; 4096];
                    let (offset, length) = loop {
                        let n = socket.read(&mut buffer).await.unwrap(); assert!(n > 0); request.extend_from_slice(&buffer[..n]);
                        if let Some(offset) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                            let headers = String::from_utf8_lossy(&request[..offset]);
                            let length: usize = headers.lines().find_map(|l|l.to_ascii_lowercase().strip_prefix("content-length:").map(|v|v.trim().parse().unwrap())).unwrap();
                            break (offset + 4, length);
                        }
                    };
                    while request.len() < offset + length { let n = socket.read(&mut buffer).await.unwrap(); assert!(n > 0); request.extend_from_slice(&buffer[..n]); }
                    let value: serde_json::Value = serde_json::from_slice(&request[offset..offset+length]).unwrap();
                    let second_step = value["messages"].as_array().unwrap().iter().any(|m|m["role"] == "tool");
                    observed.lock().push(value);
                    if !second_step { ready.send(()).await.unwrap(); release.acquire().await.unwrap().forget(); }
                    let payload = if second_step { json!({"choices":[{"message":{"role":"assistant","content":"complete"},"finish_reason":"stop"}]}) }
                        else { json!({"choices":[{"message":{"role":"assistant","content":"","tool_calls":[{"id":"call_one","type":"function","function":{"name":"fixture_noop","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}) };
                    let body = payload.to_string();
                    socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).as_bytes()).await.unwrap();
                }));
            }
            for worker in workers {
                worker.await.unwrap();
            }
        });
        let fixture = crate::tests::RuntimeFixture::new(crate::SessionSettings::default(), false);
        let runtime = fixture.runtime.clone();
        std::fs::write(
            runtime.outer_home.join("credentials.env"),
            "KEI_API_KEY=offline-fixture\n",
        )
        .unwrap();
        let mut drains = Vec::new();
        let mut turn_sessions = Vec::new();
        for (family, model, role) in [
            (crate::ModelFamily::Gpt, "model-a", "role-alpha"),
            (crate::ModelFamily::Gemini, "model-b", "role-beta"),
        ] {
            let mut profile = crate::ModelProfileConfig::new(family, model);
            profile.base_url = Some(format!("http://{address}/v1"));
            profile.instructions = role.into();
            let agent = AgentLoop::with_profile(runtime.clone(), profile).unwrap();
            let (tx, mut rx) = mpsc::channel(256);
            let session = runtime.sessions.create();
            turn_sessions.push((session.clone(), family.id(), model));
            agent
                .run_turn(session, "Execute fixture task".into(), tx)
                .await
                .unwrap();
            drains.push(tokio::spawn(async move {
                while let Some(event) = rx.recv().await {
                    assert!(!matches!(event, AgentEvent::Error(_)), "{event:?}");
                    if matches!(event, AgentEvent::Done) {
                        break;
                    }
                }
            }));
        }
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            ready_rx.recv().await.unwrap();
            ready_rx.recv().await.unwrap();
        })
        .await
        .unwrap();
        runtime.llm.set_model("mutated-global-model");
        runtime
            .prompt
            .write()
            .set_section("agent_role", "global-poison");
        release.add_permits(2);
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            for drain in drains {
                drain.await.unwrap();
            }
        })
        .await
        .unwrap();
        let requests = observed.lock().clone();
        assert_eq!(requests.len(), 4);
        for model in ["model-a", "model-b"] {
            let matching: Vec<_> = requests.iter().filter(|v| v["model"] == model).collect();
            assert_eq!(matching.len(), 2);
            for request in matching {
                let system = request["messages"][0]["content"].as_str().unwrap();
                assert!(system.contains(if model == "model-a" {
                    "role-alpha"
                } else {
                    "role-beta"
                }));
                assert!(!system.contains(if model == "model-a" {
                    "role-beta"
                } else {
                    "role-alpha"
                }));
                assert!(!system.contains("global-poison"));
            }
        }
        for (session, family, model) in &turn_sessions {
            let saved = Session::load_json(
                &runtime
                    .outer_home
                    .join("sessions")
                    .join(format!("{}.json", session.read().id)),
            )
            .unwrap();
            let start = saved
                .events
                .iter()
                .find_map(|event| match event {
                    SessionEvent::TurnStart {
                        profile_id,
                        model_id,
                        family,
                        ..
                    } => Some((profile_id, model_id, family)),
                    _ => None,
                })
                .unwrap();
            assert_eq!(start.0.as_deref(), Some(*family));
            assert_eq!(start.1.as_deref(), Some(*model));
            assert_eq!(start.2.as_deref(), Some(*family));
        }
        // Switching the same session to another profile must not rewrite its first turn.
        let mut profile = crate::ModelProfileConfig::new(crate::ModelFamily::Gemini, "model-b");
        profile.base_url = Some(format!("http://{address}/v1"));
        let agent = AgentLoop::with_profile(runtime.clone(), profile).unwrap();
        let (tx, mut rx) = mpsc::channel(256);
        agent
            .run_turn(
                turn_sessions[0].0.clone(),
                "Continue with the second model".into(),
                tx,
            )
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while let Some(event) = rx.recv().await {
                assert!(!matches!(event, AgentEvent::Error(_)), "{event:?}");
                if matches!(event, AgentEvent::Done) {
                    break;
                }
            }
            server.await.unwrap();
        })
        .await
        .unwrap();
        let saved = Session::load_json(
            &runtime
                .outer_home
                .join("sessions")
                .join(format!("{}.json", turn_sessions[0].0.read().id)),
        )
        .unwrap();
        let identities: Vec<_> = saved
            .events
            .iter()
            .filter_map(|event| match event {
                SessionEvent::TurnStart {
                    profile_id,
                    model_id,
                    family,
                    ..
                } => Some((
                    profile_id.as_deref(),
                    model_id.as_deref(),
                    family.as_deref(),
                )),
                _ => None,
            })
            .collect();
        assert_eq!(
            identities,
            vec![
                (Some("gpt"), Some("model-a"), Some("gpt")),
                (Some("gemini"), Some("model-b"), Some("gemini"))
            ]
        );
    }
}
