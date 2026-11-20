//! Durable multi-agent workspaces. File snapshots never touch the user's Git index.
use crate::{AgentEvent, AgentHandle, AgentLoop, Runtime};
use anyhow::{bail, ensure, Context, Result};
use aris_protocol::{GoalSpec, RetryPolicy, TaskRecord, TaskState, TeamTaskOwner};
use chrono::Utc;
use fs2::FileExt;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, OpenOptions},
    path::{Component, Path, PathBuf},
    sync::{Arc, Weak},
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, Notify};
use tokio::task::JoinSet;
use uuid::Uuid;

/// Automatic administration is bounded independently of manually created teams.
pub const ADMIN_WORKER_LIMIT: usize = 8;
const FINALIZATION_WINDOW_SECS: u64 = 480;

fn now() -> String {
    Utc::now().to_rfc3339()
}
fn new_id() -> String {
    Uuid::new_v4().to_string()
}
fn pending() -> String {
    "pending".into()
}

#[cfg(windows)]
fn team_replace_conflict(error: &std::io::Error, target: &Path) -> bool {
    use std::os::windows::fs::OpenOptionsExt;
    const ERROR_ACCESS_DENIED: i32 = 5;
    const ERROR_SHARING_VIOLATION: i32 = 32;
    const ERROR_LOCK_VIOLATION: i32 = 33;
    const DELETE_ACCESS: u32 = 0x0001_0000;

    match error.raw_os_error() {
        Some(ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION) => true,
        Some(ERROR_ACCESS_DENIED) => {
            // MoveFileEx can report access denied while a normal reader omits
            // FILE_SHARE_DELETE. Distinguish this from a read-only file or ACL.
            if !fs::metadata(target)
                .is_ok_and(|meta| meta.is_file() && !meta.permissions().readonly())
            {
                return false;
            }
            match OpenOptions::new().access_mode(DELETE_ACCESS).open(target) {
                Ok(_) => true, // The conflicting handle may already have closed.
                Err(probe) => matches!(
                    probe.raw_os_error(),
                    Some(ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION)
                ),
            }
        }
        _ => false,
    }
}

fn replace_team_state(staged: &Path, target: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    for delay_ms in [10, 20, 40, 80, 160, 320] {
        match fs::rename(staged, target) {
            Err(error) if team_replace_conflict(&error, target) => {
                std::thread::sleep(Duration::from_millis(delay_ms));
            }
            result => return result,
        }
    }
    fs::rename(staged, target)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamAgent {
    #[serde(default = "new_id")]
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub role: String,
    pub profile_id: String,
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub session_id: String,
    /// Stable native identity; the existing session is the subagent conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<crate::synthesis::DeliveryKind>,
    #[serde(default = "pending")]
    pub status: String,
    #[serde(default)]
    pub output: String,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamEvent {
    pub at: String,
    pub agent_id: Option<String>,
    pub kind: String,
    pub message: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Handoff {
    pub id: String,
    pub from: String,
    pub to: String,
    pub instruction: String,
    pub summary: String,
    pub at: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamMessage {
    pub id: String,
    pub agent_id: Option<String>,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub at: String,
    #[serde(default)]
    pub queued: bool,
    #[serde(default = "default_cascade")]
    pub cascade: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotFile {
    pub path: String,
    pub exists: bool,
    pub size: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileCheckpoint {
    pub id: String,
    pub label: String,
    pub at: String,
    pub files: Vec<SnapshotFile>,
}
/// The controller has a durable conversation separate from its delegated work.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamAdmin {
    pub request_id: String,
    pub session_id: String,
    pub profile_id: String,
    pub model_id: String,
    pub family: String,
    pub phase: String,
    pub response: String,
    pub error: Option<String>,
    pub requested_effort: Option<String>,
    pub applied_effort: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamExecutionBudget {
    pub agent_ids: Vec<String>,
    pub max_agent_runs: usize,
    pub max_steps_per_agent: usize,
    pub max_tool_calls_per_agent: usize,
    pub max_tokens_per_step: u32,
    pub max_parallel: usize,
    pub max_runtime_secs: u64,
    /// Minimum time from the first eligible publication turn. Older budgets
    /// keep their original shared deadline.
    #[serde(default)]
    pub finalization_window_secs: u64,
}

impl TeamExecutionBudget {
    fn role_deadline(
        &self,
        started: Instant,
        wave_started: Instant,
        role: &TeamAgent,
        publication_deadline: &mut Option<Instant>,
    ) -> Instant {
        let base = started + Duration::from_secs(self.max_runtime_secs);
        if role.delivery != Some(crate::synthesis::DeliveryKind::Final)
            || self.finalization_window_secs == 0
        {
            return base;
        }
        if let Some(deadline) = *publication_deadline {
            return deadline;
        }
        if wave_started >= base {
            return base;
        }
        let window =
            Duration::from_secs(self.finalization_window_secs.min(FINALIZATION_WINDOW_SECS));
        let deadline = base.max(wave_started + window);
        *publication_deadline = Some(deadline);
        deadline
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminAssignment {
    pub agent_id: String,
    pub instruction: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Team {
    pub id: String,
    pub name: String,
    pub goal: String,
    pub status: String,
    pub agents: Vec<TeamAgent>,
    pub files: Vec<String>,
    pub handoffs: Vec<Handoff>,
    #[serde(default)]
    pub messages: Vec<TeamMessage>,
    pub checkpoints: Vec<FileCheckpoint>,
    pub events: Vec<TeamEvent>,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admin: Option<TeamAdmin>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_budget: Option<TeamExecutionBudget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_agent_ids: Option<Vec<String>>,
    #[serde(default)]
    pub tunnels: Vec<crate::tunnel::AgentTunnel>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub tunnel_turns: HashMap<String, u8>,
}

/// A controller is identified by its durable session, never its role or name.
pub fn admin_workers(team: &Team) -> impl Iterator<Item = &TeamAgent> {
    let controller = team
        .admin
        .as_ref()
        .map(|admin| admin.session_id.as_str())
        .filter(|session| !session.is_empty());
    team.agents
        .iter()
        .filter(move |agent| Some(agent.session_id.as_str()) != controller)
}

/// Larger manual teams may dispatch existing workers without gaining new ones.
pub fn admin_worker_capacity(team: &Team) -> usize {
    ADMIN_WORKER_LIMIT.saturating_sub(admin_workers(team).count())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamSpec {
    pub name: String,
    pub goal: String,
    pub agents: Vec<TeamAgent>,
    #[serde(default)]
    pub files: Vec<String>,
}

#[derive(Default)]
struct ActiveTeam {
    wakeup: Arc<Notify>,
    handles: HashMap<String, AgentHandle>,
}

pub struct TeamStore {
    root: PathBuf,
    workspace: PathBuf,
    transaction: Mutex<()>,
    active_runs: Mutex<HashMap<String, ActiveTeam>>,
    live_outputs: Mutex<HashMap<(String, String), String>>,
    /// Disposable lookup cache; Team.agents remains the only role registry.
    session_ownership: Mutex<HashMap<String, bool>>,
    runtime: Mutex<Weak<Runtime>>,
}

impl TeamStore {
    pub fn open(root: PathBuf, workspace: PathBuf) -> Result<Self> {
        fs::create_dir_all(&root)?;
        Ok(Self {
            root,
            workspace: workspace.canonicalize()?,
            transaction: Mutex::new(()),
            active_runs: Mutex::new(HashMap::new()),
            live_outputs: Mutex::new(HashMap::new()),
            session_ownership: Mutex::new(HashMap::new()),
            runtime: Mutex::new(Weak::new()),
        })
    }
    fn directory(&self, id: &str) -> Result<PathBuf> {
        Uuid::parse_str(id).context("invalid team id")?;
        Ok(self.root.join(id))
    }
    fn read(&self, id: &str) -> Result<Team> {
        Ok(serde_json::from_slice(&fs::read(
            self.directory(id)?.join("team.json"),
        )?)?)
    }
    fn state_lock(&self, id: &str) -> Result<File> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.directory(id)?.join("state.lock"))?;
        file.lock_exclusive()?;
        Ok(file)
    }
    fn artifact_leases(&self, team: &Team) -> Result<Vec<File>> {
        let directory = self.root.join("file-leases");
        fs::create_dir_all(&directory)?;
        let mut names = team
            .files
            .iter()
            .map(|f| f.replace('\\', "/").to_lowercase())
            .collect::<Vec<_>>();
        names.sort();
        names.dedup();
        let mut leases = vec![];
        for name in names {
            let hash = format!("{:x}", Sha256::digest(name.as_bytes()));
            let file = OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(directory.join(hash))?;
            file.try_lock_exclusive().with_context(|| {
                format!("Another workflow is using {name}; wait for it to finish")
            })?;
            leases.push(file);
        }
        Ok(leases)
    }
    fn save(&self, team: &Team) -> Result<()> {
        let dir = self.directory(&team.id)?;
        fs::create_dir_all(&dir)
            .with_context(|| format!("create team directory {}", dir.display()))?;
        let target = dir.join("team.json");
        let temp = dir.join(format!("{}.tmp", new_id()));
        let bytes = serde_json::to_vec_pretty(team)?;
        let result = fs::write(&temp, bytes)
            .map_err(|e| {
                let message = format!("stage team state {}: {e}", temp.display());
                anyhow::Error::new(e).context(message)
            })
            .and_then(|()| {
                replace_team_state(&temp, &target).map_err(|e| {
                    let message = format!("replace team state {}: {e}", target.display());
                    anyhow::Error::new(e).context(message)
                })
            });
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }
    fn run_lock(&self, id: &str) -> Result<File> {
        let file = self.open_run_lock(id)?;
        file.try_lock_exclusive()
            .context("team is already running in another process")?;
        Ok(file)
    }
    fn open_run_lock(&self, id: &str) -> Result<File> {
        Ok(OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.directory(id)?.join("run.lock"))?)
    }
    /// A CLI observer may see a terminal state before the owner has finished
    /// persisting the review job and released the controller lease.
    pub async fn wait_for_controller(&self, id: &str) -> Result<()> {
        let file = self.open_run_lock(id)?;
        tokio::task::spawn_blocking(move || fs2::FileExt::lock_shared(&file)).await??;
        Ok(())
    }
    pub fn get(&self, id: &str) -> Result<Team> {
        let _guard = self.transaction.lock();
        let _state = self.state_lock(id)?;
        let mut team = self.read(id)?;
        if matches!(team.status.as_str(), "running" | "cancelling" | "planning")
            && !self.active_runs.lock().contains_key(id)
        {
            if let Ok(_lease) = self.run_lock(id) {
                let runtime = self.runtime.lock().upgrade();
                if let Some(runtime) = &runtime {
                    let tasks = &runtime.tasks;
                    for agent in &team.agents {
                        if let Some(task) = agent.task_id.as_deref().and_then(|id| tasks.task(id)) {
                            if task.owner.as_ref().is_some_and(|owner| {
                                owner.team_id == team.id
                                    && owner.agent_id == agent.id
                                    && Some(&owner.subagent_id) == agent.subagent_id.as_ref()
                            }) {
                                tasks.interrupt_team_task(&task.id)?;
                            }
                        }
                    }
                }
                team.status = "failed".into();
                crate::tunnel::close_all(
                    &mut team,
                    "interrupted",
                    "Coordinator stopped; transcript retained for the next explicit dispatch",
                );
                if let Some(admin) = &mut team.admin {
                    if admin.phase == "planning" {
                        admin.phase = "failed".into();
                        admin.error =
                            Some("Controller process stopped; send the message again.".into());
                    }
                }
                for agent in &mut team.agents {
                    if agent.status == "running" {
                        agent.status = "failed".into();
                        agent.error = Some(
                            "Previous process stopped; run again to resume this workspace.".into(),
                        );
                    }
                }
                Self::event(
                    &mut team,
                    None,
                    "interrupted",
                    "Previous process stopped. Completed outputs and checkpoints were retained.",
                );
                if let Some(runtime) = &runtime {
                    crate::review_intents::stage_team(
                        runtime,
                        &team,
                        &team.events[team.events.len() - 1..],
                    )?;
                }
                self.save(&team)?;
                drop(_state);
                drop(_guard);
                if let Some(runtime) = runtime {
                    runtime.review_team_completion(&team);
                }
                return Ok(team);
            }
        }
        for agent in &mut team.agents {
            if agent.status == "running" {
                if let Some(output) = self.live_outputs.lock().get(&(id.into(), agent.id.clone())) {
                    agent.output.clone_from(output);
                }
            }
        }
        Ok(team)
    }

    pub(crate) fn has_terminal_event(
        &self,
        id: &str,
        at: &str,
        kind: &str,
    ) -> Result<Option<bool>> {
        let _guard = self.transaction.lock();
        let _state = self.state_lock(id)?;
        let lease = self.open_run_lock(id)?;
        match lease.try_lock_exclusive() {
            Ok(()) => Ok(Some(
                self.read(id)?
                    .events
                    .iter()
                    .any(|e| e.at == at && e.kind == kind),
            )),
            Err(e) if e.raw_os_error() == fs2::lock_contended_error().raw_os_error() => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    pub fn list(&self) -> Result<Vec<Team>> {
        let mut teams = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                let id = entry.file_name().to_string_lossy().into_owned();
                if Uuid::parse_str(&id).is_ok() {
                    teams.push(self.get(&id)?);
                }
            }
        }
        teams.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(teams)
    }
    pub(crate) fn mutate(
        &self,
        id: &str,
        update: impl FnOnce(&mut Team) -> Result<()>,
    ) -> Result<Team> {
        let _guard = self.transaction.lock();
        let _state = self.state_lock(id)?;
        let mut team = self.read(id)?;
        let before = team.events.len();
        update(&mut team)?;
        if let Some(new_events) = team.events.get(before..) {
            if new_events.iter().any(crate::review_intents::is_team_end) {
                if let Some(runtime) = self.runtime.lock().upgrade() {
                    crate::review_intents::stage_team(&runtime, &team, new_events)?;
                }
            }
        }
        self.save(&team)?;
        self.cache_team_sessions(&team);
        Ok(team)
    }
    pub(crate) fn event(team: &mut Team, agent_id: Option<&str>, kind: &str, message: &str) {
        team.events.push(TeamEvent {
            at: now(),
            agent_id: agent_id.map(str::to_owned),
            kind: kind.into(),
            message: message.into(),
        });
    }
    /// Wake the sole coordinator after actionable peer work is durable. A permit
    /// also covers a send between the coordinator's last read and its next wait.
    pub(crate) fn wake_coordinator(&self, id: &str) {
        if let Some(run) = self.active_runs.lock().get(id) {
            run.wakeup.notify_one();
        }
    }
    /// Team.agents is the durable subagent registry. No second conversation or
    /// shadow role store is created. Legacy roles acquire an identity in place.
    fn register_workers(team: &mut Team) {
        let parent = team.admin.as_ref().map(|admin| admin.session_id.as_str());
        for agent in &mut team.agents {
            if Some(agent.session_id.as_str()) == parent {
                continue;
            }
            if agent.subagent_id.is_none() {
                agent.subagent_id = Some(new_id());
            }
        }
    }
    fn task_owner(&self, team: &Team, agent: &TeamAgent) -> Result<TeamTaskOwner> {
        Ok(TeamTaskOwner {
            subagent_id: agent
                .subagent_id
                .clone()
                .context("subagent is not registered")?,
            team_id: team.id.clone(),
            agent_id: agent.id.clone(),
            parent_session_id: team.admin.as_ref().map(|admin| admin.session_id.clone()),
            workspace: self.workspace.to_string_lossy().into_owned(),
        })
    }
    /// Ordinary chat/task entry points must not bypass a team's frozen model,
    /// dependency ordering, artifact leases or cancellation boundaries.
    pub fn owns_session(&self, session_id: &str) -> Result<bool> {
        if let Some(owned) = self.session_ownership.lock().get(session_id) {
            return Ok(*owned);
        }
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let id = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type()?.is_dir() && Uuid::parse_str(&id).is_ok() {
                let Ok(team) = self.read(&id) else { continue };
                if team
                    .agents
                    .iter()
                    .any(|agent| agent.session_id == session_id)
                    || team
                        .admin
                        .as_ref()
                        .is_some_and(|admin| admin.session_id == session_id)
                {
                    self.cache_team_sessions(&team);
                    return Ok(true);
                }
            }
        }
        self.session_ownership
            .lock()
            .insert(session_id.into(), false);
        Ok(false)
    }
    fn cache_team_sessions(&self, team: &Team) {
        let mut cache = self.session_ownership.lock();
        for agent in &team.agents {
            cache.insert(agent.session_id.clone(), true);
        }
        if let Some(admin) = &team.admin {
            cache.insert(admin.session_id.clone(), true);
        }
    }
    /// Run before the ordinary scheduler starts. Historical task records are
    /// linked by their already-persisted session, without replaying any work.
    pub(crate) fn reconcile_tasks(&self, tasks: &crate::TaskStore) -> Result<()> {
        let mut ids = Vec::new();
        let mut historical: HashMap<String, Vec<TaskRecord>> = HashMap::new();
        for task in tasks.tasks() {
            if let Some(session_id) = &task.session_id {
                historical.entry(session_id.clone()).or_default().push(task);
            }
        }
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let id = entry.file_name().to_string_lossy().into_owned();
            if !entry.file_type()?.is_dir() || Uuid::parse_str(&id).is_err() {
                continue;
            }
            let _guard = self.transaction.lock();
            let _state = self.state_lock(&id)?;
            let mut team = match self.read(&id) {
                Ok(team) => team,
                Err(error) => {
                    tracing::warn!(team_id=%id, %error, "skipping unreadable historical team");
                    continue;
                }
            };
            let before = team
                .agents
                .iter()
                .map(|agent| {
                    (
                        agent.subagent_id.clone(),
                        agent.task_id.clone(),
                        agent.run_id.clone(),
                    )
                })
                .collect::<Vec<_>>();
            Self::register_workers(&mut team);
            let owners: HashMap<_, _> = team
                .agents
                .iter()
                .filter(|agent| agent.subagent_id.is_some())
                .map(|agent| Ok((agent.id.clone(), self.task_owner(&team, agent)?)))
                .collect::<Result<_>>()?;
            for agent in &mut team.agents {
                let Some(owner) = owners.get(&agent.id) else {
                    continue;
                };
                let Some(history) = historical.get(&agent.session_id) else {
                    continue;
                };
                for task in history {
                    if let Some(existing) = &task.owner {
                        ensure!(
                            existing.subagent_id == owner.subagent_id
                                && existing.team_id == owner.team_id
                                && existing.agent_id == owner.agent_id,
                            "task belongs to another subagent"
                        );
                    } else {
                        tasks.update_task(&task.id, |record| record.owner = Some(owner.clone()))?;
                    }
                }
                if let Some(task) = history.iter().max_by(|left, right| {
                    left.updated_at
                        .cmp(&right.updated_at)
                        .then_with(|| left.id.cmp(&right.id))
                }) {
                    agent.task_id = Some(task.id.clone());
                    agent.run_id = tasks.latest_run_for_task(&task.id).map(|run| run.id);
                }
            }
            let changed = team.agents.iter().zip(before).any(|(agent, previous)| {
                (
                    agent.subagent_id.clone(),
                    agent.task_id.clone(),
                    agent.run_id.clone(),
                ) != previous
            });
            if changed {
                self.save(&team)?;
            }
            self.cache_team_sessions(&team);
            ids.push(team.id.clone());
        }
        // Identity migration happens first, so get() can reconcile the real
        // task/run only while it owns the released execution lock.
        for id in ids {
            self.get(&id)?;
        }
        Ok(())
    }
    pub(crate) fn attach_runtime(&self, runtime: &Arc<Runtime>) {
        *self.runtime.lock() = Arc::downgrade(runtime);
    }
    fn prepare_task(
        &self,
        runtime: &Runtime,
        team: &Team,
        agent: &TeamAgent,
        prompt: &str,
    ) -> Result<TaskRecord> {
        let owner = self.task_owner(team, agent)?;
        let reusable = agent
            .task_id
            .as_deref()
            .and_then(|id| runtime.tasks.task(id))
            .filter(|task| {
                task.owner.as_ref() == Some(&owner)
                    && task.session_id.as_deref() == Some(&agent.session_id)
                    && task.goal.outcome == prompt
                    && matches!(
                        task.state,
                        TaskState::Queued | TaskState::Paused | TaskState::Failed
                    )
            });
        let task = if let Some(task) = reusable {
            task
        } else {
            let mut task = TaskRecord::new_with_retry_policy(
                GoalSpec::new(prompt),
                RetryPolicy {
                    max_attempts: runtime.config.agent.retry_max_attempts,
                    backoff_secs: runtime.config.agent.retry_backoff_secs,
                    max_backoff_secs: runtime.config.agent.retry_max_backoff_secs,
                },
            )
            .map_err(anyhow::Error::msg)?;
            task.session_id = Some(agent.session_id.clone());
            task.parent_task_id = owner
                .parent_session_id
                .as_deref()
                .and_then(|id| runtime.tasks.task_for_session(id))
                .map(|parent| parent.id);
            task.owner = Some(owner);
            runtime.tasks.create_task(task)?
        };
        self.mutate(&team.id, |team| {
            let current = team
                .agents
                .iter_mut()
                .find(|candidate| candidate.id == agent.id)
                .context("agent missing")?;
            current.task_id = Some(task.id.clone());
            current.run_id = None;
            Ok(())
        })?;
        Ok(task)
    }
    pub fn create(&self, runtime: &Runtime, mut spec: TeamSpec) -> Result<Team> {
        ensure!(!spec.name.trim().is_empty(), "team name is required");
        ensure!(!spec.goal.trim().is_empty(), "goal is required");
        ensure!(
            !spec.agents.is_empty() && spec.agents.len() <= 32,
            "a team needs 1–32 agents"
        );
        validate_graph(&spec.agents)?;
        let kei = crate::kei::load_kei_config(&runtime.outer_home, &runtime.config.kei)?;
        for agent in &mut spec.agents {
            ensure!(!agent.name.trim().is_empty(), "agent name is required");
            kei.profile(&agent.profile_id)?;
            agent.files = self.validate_files(&agent.files)?;
            spec.files.extend(agent.files.clone());
            // Client-provided session ids and execution state are never trusted.
            let session = runtime.sessions.create();
            {
                let mut s = session.write();
                s.name = Some(format!("{} / {}", spec.name, agent.name));
                s.cwd = Some(self.workspace.to_string_lossy().into());
            }
            runtime.sessions.persist_now_result(&session)?;
            agent.session_id = session.read().id.clone();
            agent.subagent_id = None;
            agent.task_id = None;
            agent.run_id = None;
            agent.status = pending();
            agent.output.clear();
            agent.error = None;
        }
        let mut team = Team {
            id: new_id(),
            name: spec.name,
            goal: spec.goal,
            status: "idle".into(),
            agents: spec.agents,
            files: self.validate_files(&spec.files)?,
            handoffs: vec![],
            messages: vec![],
            checkpoints: vec![],
            events: vec![],
            created_at: now(),
            admin: None,
            execution_budget: None,
            dispatch_agent_ids: None,
            tunnels: vec![],
            tunnel_turns: HashMap::new(),
        };
        Self::register_workers(&mut team);
        Self::event(&mut team, None, "created", "Shared workspace created");
        let _guard = self.transaction.lock();
        self.save(&team)?;
        self.cache_team_sessions(&team);
        Ok(team)
    }
    fn validate_files(&self, files: &[String]) -> Result<Vec<String>> {
        ensure!(files.len() <= 128, "at most 128 managed artifact files");
        let mut result = Vec::new();
        for path in files {
            self.artifact_path(path)?;
            let normalized = path.replace('\\', "/");
            let path = Path::new(&normalized)
                .components()
                .map(|part| part.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            if !result
                .iter()
                .any(|p: &String| p.eq_ignore_ascii_case(&path))
            {
                result.push(path);
            }
        }
        Ok(result)
    }
    fn artifact_path(&self, raw: &str) -> Result<PathBuf> {
        ensure!(
            !raw.is_empty() && raw.len() <= 1024 && !raw.contains([':', '\0']),
            "invalid artifact path"
        );
        let normalized = raw.replace('\\', "/");
        let path = Path::new(&normalized);
        ensure!(
            path.components().all(|c| matches!(c, Component::Normal(_))),
            "artifact paths must be workspace-relative without .."
        );
        for part in path.components() {
            let value = part.as_os_str().to_string_lossy().to_lowercase();
            ensure!(
                !value.ends_with(['.', ' '])
                    && !matches!(
                        value.split('.').next(),
                        Some(
                            "con"
                                | "prn"
                                | "aux"
                                | "nul"
                                | "com1"
                                | "com2"
                                | "com3"
                                | "com4"
                                | "lpt1"
                                | "lpt2"
                                | "lpt3"
                        )
                    ),
                "artifact path has an ambiguous platform filename"
            );
            ensure!(
                !matches!(
                    value.as_str(),
                    ".git" | ".aris" | ".dsh-rust" | "target" | "node_modules"
                ) && !value.starts_with(".env")
                    && value != "credentials.env",
                "artifact path includes a private or generated directory"
            );
        }
        let full = self.workspace.join(path);
        let mut check = self.workspace.clone();
        for part in path.components() {
            check.push(part);
            if let Ok(meta) = fs::symlink_metadata(&check) {
                ensure!(
                    !meta.file_type().is_symlink(),
                    "artifact symlinks are not supported"
                );
                ensure!(
                    check.canonicalize()?.starts_with(&self.workspace),
                    "artifact escapes workspace"
                );
            }
        }
        ensure!(!full.is_dir(), "declare individual files, not directories");
        Ok(full)
    }
    fn capture(&self, team: &mut Team, label: &str) -> Result<()> {
        let id = new_id();
        let directory = self.directory(&team.id)?.join("snapshots").join(&id);
        fs::create_dir_all(&directory)?;
        let mut files = vec![];
        for (index, relative) in team.files.iter().enumerate() {
            let path = self.artifact_path(relative)?;
            let exists = path.try_exists()?;
            let size = if exists {
                fs::metadata(&path)?.len()
            } else {
                0
            };

            if exists {
                fs::copy(&path, directory.join(index.to_string()))?;
            }
            files.push(SnapshotFile {
                path: relative.clone(),
                exists,
                size,
            });
        }
        team.checkpoints.push(FileCheckpoint {
            id,
            label: label.into(),
            at: now(),
            files,
        });
        Self::event(team, None, "checkpoint", label);
        Ok(())
    }
    pub fn checkpoint(&self, id: &str, label: &str) -> Result<Team> {
        let _lease = self.run_lock(id)?;
        let _files = self.artifact_leases(&self.read(id)?)?;
        self.mutate(id, |team| {
            ensure!(
                team.status != "running",
                "stop the team before checkpointing"
            );
            self.capture(team, label)
        })
    }
    pub fn restore(&self, id: &str, checkpoint_id: &str) -> Result<Team> {
        Uuid::parse_str(checkpoint_id).context("invalid checkpoint id")?;
        let _lease = self.run_lock(id)?;
        let _files = self.artifact_leases(&self.read(id)?)?;
        self.mutate(id, |team| {
            ensure!(
                team.status != "running",
                "stop the team before restoring files"
            );
            let snapshot = team
                .checkpoints
                .iter()
                .find(|s| s.id == checkpoint_id)
                .cloned()
                .context("checkpoint not found")?;
            let directory = self.directory(id)?.join("snapshots").join(checkpoint_id);
            // Preflight every file before touching the workspace.
            for (index, file) in snapshot.files.iter().enumerate() {
                self.artifact_path(&file.path)?;
                if file.exists {
                    ensure!(
                        fs::metadata(directory.join(index.to_string()))?.len() == file.size,
                        "checkpoint data is incomplete"
                    );
                }
            }
            self.capture(team, "Before restore (undo)")?;
            // Keep the undo checkpoint discoverable even if a later file write
            // fails (for example, a destination is held open by another app).
            self.save(team)?;
            for (index, file) in snapshot.files.iter().enumerate() {
                let path = self.artifact_path(&file.path)?;
                if file.exists {
                    fs::create_dir_all(path.parent().context("artifact parent missing")?)?;
                    let temp = path.with_extension(format!("{}.tmp", new_id()));
                    fs::copy(directory.join(index.to_string()), &temp)?;
                    fs::rename(temp, path)?;
                } else if path.exists() {
                    fs::remove_file(path)?;
                }
            }
            Self::event(
                team,
                None,
                "restored",
                &format!(
                    "Restored {}. Conversation history was retained.",
                    snapshot.label
                ),
            );
            Ok(())
        })
    }
    pub fn cancel(&self, id: &str) -> Result<Team> {
        let team = self.mutate(id, |team| {
            ensure!(
                matches!(team.status.as_str(), "running" | "planning"),
                "team is not running"
            );
            team.status = "cancelling".into();
            Self::event(
                team,
                None,
                "cancelling",
                "Cancellation requested; waiting for active tools to stop",
            );
            Ok(())
        })?;
        if let Some(run) = self.active_runs.lock().get(id) {
            for handle in run.handles.values() {
                handle.cancel();
            }
        }
        Ok(team)
    }
    pub fn handoff(&self, id: &str, from: &str, to: &str, instruction: &str) -> Result<Team> {
        let _lease = self.run_lock(id)?;
        self.mutate(id, |team| {
            ensure!(
                !matches!(team.status.as_str(), "running" | "cancelling"),
                "wait for the team to stop before handoff"
            );
            ensure!(from != to, "invalid handoff");
            let source = team
                .agents
                .iter()
                .find(|a| a.id == from)
                .context("source agent not found")?;
            ensure!(
                source.status == "completed",
                "source agent has no completed handoff"
            );
            let summary = source.output.clone();
            let target = team
                .agents
                .iter_mut()
                .find(|a| a.id == to)
                .context("target agent not found")?;
            target.status = pending();
            target.error = None;
            // Downstream outputs were based on the old result and must be rebuilt.
            let mut invalidated = HashSet::from([to.to_owned()]);
            loop {
                let before = invalidated.len();
                for agent in &mut team.agents {
                    if agent.depends_on.iter().any(|d| invalidated.contains(d)) {
                        invalidated.insert(agent.id.clone());
                        agent.status = pending();
                        agent.error = None;
                    }
                }
                if before == invalidated.len() {
                    break;
                }
            }
            team.handoffs.push(Handoff {
                id: new_id(),
                from: from.into(),
                to: to.into(),
                instruction: instruction.into(),
                summary,
                at: now(),
            });
            team.status = "idle".into();
            Self::event(
                team,
                Some(to),
                "handoff",
                &format!("{from} → {to}: {instruction}"),
            );
            Ok(())
        })
    }
    /// Continue a team conversation, or one agent and its dependent subflows.
    /// Durable sessions remain intact so each agent can see its earlier work.
    pub fn message(&self, id: &str, agent_id: Option<&str>, text: &str) -> Result<Team> {
        self.submit_message(id, agent_id, text, true)
    }
    pub fn chat_message(&self, id: &str, agent_id: Option<&str>, text: &str) -> Result<Team> {
        self.submit_message(id, agent_id, text, false)
    }
    pub fn set_model(
        &self,
        runtime: &Runtime,
        id: &str,
        agent_id: &str,
        profile_id: &str,
    ) -> Result<Team> {
        let config = crate::kei::load_kei_config(&runtime.outer_home, &runtime.config.kei)?;
        config
            .profile(profile_id)?
            .resolve(&config, &runtime.llm.config(), &runtime.outer_home)?;
        let _lease = self.run_lock(id)?;
        self.mutate(id, |team| {
            ensure!(
                !matches!(team.status.as_str(), "running" | "cancelling"),
                "stop the current run before switching models"
            );
            let agent = team
                .agents
                .iter_mut()
                .find(|agent| agent.id == agent_id)
                .context("agent not found")?;
            agent.profile_id = profile_id.into();
            Self::event(team, Some(agent_id), "model_changed", profile_id);
            Ok(())
        })
    }
    /// Reserve the same cross-process lease used by execution before any model
    /// request. Other controller requests and structural mutations cannot race it.
    pub fn admin_lease(&self, id: &str) -> Result<File> {
        self.get(id)?;
        self.run_lock(id)
    }
    pub fn reserve_admin(&self, id: &str, admin: TeamAdmin, lease: File) -> Result<(Team, File)> {
        let team = self.mutate(id, |team| {
            ensure!(
                !matches!(team.status.as_str(), "running" | "cancelling" | "planning"),
                "Wait for the current team operation before asking the admin"
            );
            ensure!(
                !team.messages.iter().any(|m| m.queued),
                "Run or resolve queued team messages first"
            );
            if let Some(controller) = team
                .agents
                .iter_mut()
                .find(|a| a.session_id == admin.session_id)
            {
                controller.profile_id.clone_from(&admin.profile_id);
                controller.subagent_id = None;
                controller.task_id = None;
                controller.run_id = None;
            }
            team.admin = Some(admin);
            team.status = "planning".into();
            Self::event(
                team,
                None,
                "admin_started",
                "Admin is answering or planning this message",
            );
            Ok(())
        })?;
        Ok((team, lease))
    }

    /// Called only by the controller that owns the run lease. Existing roles,
    /// profiles, dependencies and sessions are never rewritten by a model plan.
    pub fn finish_admin(
        &self,
        runtime: &Runtime,
        id: &str,
        request_id: &str,
        response: &str,
        mut additions: Vec<TeamAgent>,
        assignments: Vec<AdminAssignment>,
    ) -> Result<Team> {
        self.mutate(id, |team| {
            ensure!(
                team.status == "planning",
                "Admin was cancelled before applying its result"
            );
            let admin = team.admin.as_ref().context("admin session missing")?;
            ensure!(
                admin.request_id == request_id && admin.phase == "planning",
                "Stale admin result"
            );
            let admin_session = admin.session_id.clone();
            ensure!(
                additions.len() <= admin_worker_capacity(team),
                "Admin may add only enough roles to reach {ADMIN_WORKER_LIMIT} workers"
            );
            ensure!(
                assignments.len() <= ADMIN_WORKER_LIMIT,
                "Admin may assign at most {ADMIN_WORKER_LIMIT} workers per dispatch"
            );
            ensure!(
                team.agents.len() + additions.len() <= 32,
                "Team role limit exceeded"
            );
            let mut combined = team.agents.clone();
            combined.extend(additions.clone());
            validate_graph(&combined)?;
            let mut selected = HashSet::new();
            for assignment in &assignments {
                ensure!(
                    selected.insert(assignment.agent_id.clone()),
                    "duplicate admin assignment"
                );
                ensure!(
                    !assignment.instruction.trim().is_empty(),
                    "invalid admin assignment instructions"
                );
                let agent = combined
                    .iter()
                    .find(|a| a.id == assignment.agent_id)
                    .context("admin selected an unknown role")?;
                ensure!(
                    agent.session_id != admin_session,
                    "Admin cannot recursively delegate to itself"
                );
            }
            for agent in &combined {
                if selected.contains(&agent.id) {
                    for dep in &agent.depends_on {
                        ensure!(
                            selected.contains(dep)
                                || combined
                                    .iter()
                                    .any(|a| &a.id == dep && a.status == "completed"),
                            "A selected role depends on an unfinished role outside this dispatch"
                        );
                    }
                }
            }
            let kei = crate::kei::load_kei_config(&runtime.outer_home, &runtime.config.kei)?;
            let mut files = team.files.clone();
            for agent in &mut additions {
                ensure!(
                    selected.contains(&agent.id),
                    "New roles must have an assignment"
                );
                ensure!(
                    !agent.name.trim().is_empty() && !agent.role.trim().is_empty(),
                    "Invalid new role"
                );
                kei.profile(&agent.profile_id)?.resolve(
                    &kei,
                    &runtime.llm.config(),
                    &runtime.outer_home,
                )?;
                agent.files = self.validate_files(&agent.files)?;
                files.extend(agent.files.clone());
            }
            let files = self.validate_files(&files)?;
            for agent in &mut additions {
                let session = runtime.sessions.create();
                {
                    let mut s = session.write();
                    s.name = Some(format!("{} / {}", team.name, agent.name));
                    s.cwd = Some(self.workspace.to_string_lossy().into());
                }
                runtime.sessions.persist_now_result(&session)?;
                agent.session_id = session.read().id.clone();
                agent.subagent_id = None;
                agent.task_id = None;
                agent.run_id = None;
                agent.status = pending();
                agent.output.clear();
                agent.error = None;
            }
            team.files = files;
            for agent in &additions {
                Self::event(team, Some(&agent.id), "subagent_created", &agent.name);
            }
            team.agents.extend(additions);
            Self::register_workers(team);
            for agent in &mut team.agents {
                if agent.session_id == admin_session {
                    agent.status = "completed".into();
                    agent.output = response.into();
                    agent.error = None;
                } else if selected.contains(&agent.id) {
                    agent.status = pending();
                    agent.output.clear();
                    agent.error = None;
                }
            }
            for assignment in assignments {
                team.messages.push(TeamMessage {
                    id: new_id(),
                    agent_id: Some(assignment.agent_id),
                    text: assignment.instruction,
                    source: Some("admin".into()),
                    at: now(),
                    queued: false,
                    cascade: false,
                });
            }
            let delegated = !selected.is_empty();
            let final_owner = team.agents.iter().find(|a| {
                selected.contains(&a.id)
                    && a.delivery == Some(crate::synthesis::DeliveryKind::Final)
            });
            let compiling_delivery = selected.len() > 1 && final_owner.is_some();
            let finalization_window_secs = match final_owner {
                Some(writer)
                    if crate::synthesis::ancestors(team, writer)?
                        .iter()
                        .filter(|source| source.session_id != admin_session)
                        .count()
                        >= 2 =>
                {
                    FINALIZATION_WINDOW_SECS
                }
                _ => 0,
            };
            let max_agent_runs = selected.len() + crate::tunnel::MAX_FOLLOWUPS;
            team.dispatch_agent_ids = None;
            team.execution_budget = delegated.then(|| TeamExecutionBudget {
                agent_ids: selected.into_iter().collect(),
                max_agent_runs,
                max_steps_per_agent: if compiling_delivery { 18 } else { 12 },
                max_tool_calls_per_agent: if compiling_delivery { 40 } else { 32 },
                max_tokens_per_step: 4096,
                max_parallel: 3,
                max_runtime_secs: 900,
                finalization_window_secs,
            });
            let admin = team.admin.as_mut().unwrap();
            admin.phase = if delegated { "delegated" } else { "answered" }.into();
            admin.response = response.into();
            admin.error = None;
            team.status = if delegated {
                "planning"
            } else if team.agents.iter().any(|a| a.status != "completed") {
                "idle"
            } else {
                "completed"
            }
            .into();
            Self::event(
                team,
                None,
                if delegated {
                    "admin_delegated"
                } else {
                    "admin_answered"
                },
                response,
            );
            Ok(())
        })
    }

    pub fn fail_admin(&self, id: &str, request_id: &str, error: &str) -> Result<Team> {
        self.mutate(id, |team| {
            let cancelled = team.status == "cancelling";
            let admin = team.admin.as_mut().context("admin session missing")?;
            ensure!(admin.request_id == request_id, "Stale admin result");
            admin.phase = if cancelled { "cancelled" } else { "failed" }.into();
            admin.error = Some(error.into());
            team.status = if cancelled { "cancelled" } else { "failed" }.into();
            Self::event(team, None, "admin_stopped", error);
            Ok(())
        })
    }

    fn submit_message(
        &self,
        id: &str,
        agent_id: Option<&str>,
        text: &str,
        cascade: bool,
    ) -> Result<Team> {
        ensure!(!text.trim().is_empty(), "message is required");
        self.mutate(id, |team| {
            ensure!(team.status != "planning" && !(team.status == "cancelling" && team.admin.as_ref().is_some_and(|a| a.phase == "planning")),
                "Wait for the admin to finish planning, or cancel it first");
            // A controller can already own the lease while preparing its
            // persisted planning state. Do not accept a message in that gap.
            let _idle_lease = if !matches!(team.status.as_str(), "running" | "cancelling") {
                Some(self.run_lock(id)?)
            } else { None };
            // A direct user instruction starts its own ordinary, explicit run.
            if !matches!(team.status.as_str(), "running" | "cancelling") {
                team.execution_budget = None;
            }
            if let Some(target) = agent_id {
                ensure!(
                    team.agents.iter().any(|a| a.id == target),
                    "agent not found"
                );
            }
            let queued = matches!(team.status.as_str(), "running" | "cancelling");
            if queued {
                if let Some(budget) = &team.execution_budget {
                    ensure!(agent_id.is_some_and(|id| budget.agent_ids.iter().any(|selected| selected == id)) && !cascade,
                        "This admin run has a fixed role budget; wait or cancel before dispatching other roles");
                }
            }
            team.messages.push(TeamMessage {
                id: new_id(),
                agent_id: agent_id.map(str::to_owned),
                text: text.trim().into(),
                source: Some("user".into()),
                at: now(),
                queued: true,
                cascade,
            });
            Self::event(
                team,
                agent_id,
                if queued {
                    "message_queued"
                } else {
                    "user_message"
                },
                text.trim(),
            );
            if !queued {
                apply_queued_messages(team);
                team.status = "idle".into();
            }
            Ok(())
        })
    }
    pub fn start(self: &Arc<Self>, runtime: Arc<Runtime>, id: &str) -> Result<Team> {
        // Recover a stale process before taking the lease ourselves.
        self.get(id)?;
        let lease = self.run_lock(id)?;
        self.start_reserved(runtime, id, lease)
    }
    pub fn start_all(self: &Arc<Self>, runtime: Arc<Runtime>, id: &str) -> Result<Team> {
        self.get(id)?;
        let lease = self.run_lock(id)?;
        self.mutate(id, |team| {
            team.dispatch_agent_ids = None;
            Ok(())
        })?;
        self.start_reserved(runtime, id, lease)
    }
    /// Transfers the controller lease directly into execution with no unlock gap.
    pub fn start_reserved(
        self: &Arc<Self>,
        runtime: Arc<Runtime>,
        id: &str,
        lease: impl Into<Arc<File>>,
    ) -> Result<Team> {
        let lease = lease.into();
        let team = self.get(id)?;
        let file_leases = self.artifact_leases(&team)?;
        let scheduling = crate::reviewer_policy::for_execution(&runtime);
        let kei = crate::kei::load_kei_config(&runtime.outer_home, &runtime.config.kei)?;
        // Validate all connections before marking any stage running.
        ensure!(
            !matches!(team.status.as_str(), "running" | "cancelling")
                && (team.status != "planning"
                    || team.admin.as_ref().is_some_and(|a| a.phase == "delegated")),
            "team is already running"
        );
        for agent in &team.agents {
            if !selected_for_run(&team, agent) {
                continue;
            }
            kei.profile(&agent.profile_id)?.resolve(
                &kei,
                &runtime.llm.config(),
                &runtime.outer_home,
            )?;
        }
        let team = self.mutate(id, |team| {
            Self::register_workers(team);
            team.tunnel_turns.clear();
            apply_queued_messages(team);
            ensure!(
                team.agents
                    .iter()
                    .any(|a| selected_for_run(team, a) && a.status != "completed"),
                "all agents completed; send a message to continue"
            );
            self.capture(team, "Before run")?;
            let selected: HashSet<_> = team
                .agents
                .iter()
                .filter(|agent| selected_for_run(team, agent))
                .map(|agent| agent.id.clone())
                .collect();
            for agent in &mut team.agents {
                if agent.status != "completed" && selected.contains(&agent.id) {
                    agent.status = pending();
                    agent.error = None;
                    agent.output.clear();
                }
            }
            team.status = "running".into();
            Self::event(
                team,
                None,
                "started",
                "Executing independent agents in dependency order",
            );
            if scheduling.revision != 0 {
                Self::event(
                    team,
                    None,
                    "scheduler_policy",
                    &serde_json::to_string(&scheduling)?,
                );
            }
            Ok(())
        })?;
        let active = ActiveTeam::default();
        let wakeup = active.wakeup.clone();
        self.active_runs.lock().insert(id.into(), active);
        let store = self.clone();
        let id = id.to_owned();
        tokio::spawn(async move {
            let mut running = JoinSet::new();
            let result = store
                .drive(
                    runtime.clone(),
                    &id,
                    &wakeup,
                    &mut running,
                    scheduling.policy,
                )
                .await;
            let terminal = match result {
                Ok(team) => Ok(team),
                Err(error) => {
                    // Keep leases until active runners (including ones still opening
                    // their native task) have observed cancellation and stopped.
                    if !running.is_empty() {
                        let _ = store.cancel(&id);
                        while running.join_next().await.is_some() {}
                    }
                    store.mutate(&id, |t| {
                        t.status = "failed".into();
                        crate::tunnel::close_all(t, "interrupted", &error.to_string());
                        Self::event(t, None, "error", &error.to_string());
                        Ok(())
                    })
                }
            };
            match terminal {
                Ok(team) => runtime.review_team_completion(&team),
                Err(error) => tracing::warn!(%error, team_id=%id, "could not finalize team"),
            }
            store.active_runs.lock().remove(&id);
            drop(file_leases);
            drop(lease);
        });
        Ok(team)
    }
    async fn drive(
        self: &Arc<Self>,
        runtime: Arc<Runtime>,
        id: &str,
        wakeup: &Notify,
        running: &mut JoinSet<(String, Result<()>)>,
        scheduling: crate::reviewer_policy::SchedulingPolicy,
    ) -> Result<Team> {
        let started = Instant::now();
        let mut publication_deadline = None;
        let mut runs = 0usize;
        let mut in_flight = HashSet::new();
        let mut publication_ran = false;
        let mut final_review = false;
        loop {
            let team = self.mutate(id, |t| {
                if t.status != "cancelling" {
                    if running.is_empty() {
                        apply_queued_messages(t);
                    }
                    crate::tunnel::schedule(t);
                }
                Ok(())
            })?;
            if team.status == "cancelling" {
                while let Some(joined) = running.join_next().await {
                    joined?.1?;
                }
                let cancelled = self.mutate(id, |t| {
                    t.status = "cancelled".into();
                    crate::tunnel::close_all(t, "cancelled", "Team execution was cancelled");
                    let selected: HashSet<_> = t
                        .agents
                        .iter()
                        .filter(|agent| selected_for_run(t, agent))
                        .map(|agent| agent.id.clone())
                        .collect();
                    for a in &mut t.agents {
                        if a.status == "pending" && selected.contains(&a.id) {
                            a.status = "cancelled".into();
                        }
                    }
                    Self::event(t, None, "cancelled", "All active agents stopped");
                    self.capture(t, "After cancellation")
                })?;
                return Ok(cancelled);
            }
            // User follow-ups can change several roles' assignments. Drain
            // existing work before applying them, as for ordinary reruns.
            let ready = if team.messages.iter().any(|message| message.queued) {
                vec![]
            } else {
                ready_wave_with_policy(&team, &in_flight, scheduling)
            };
            if ready.is_empty() && running.is_empty() {
                let terminal = self.mutate(id, |t| {
                    // A message may arrive after ready_wave was computed. Keep
                    // the run alive and consume it on the next safe boundary.
                    if t.status == "cancelling" || t.messages.iter().any(|message| message.queued) {
                        return Ok(());
                    }
                    let mut complete = t
                        .agents
                        .iter()
                        .filter(|a| selected_for_run(t, a))
                        .all(|a| a.status == "completed");
                    if complete && publication_ran {
                        if let Some(delivery) = crate::synthesis::deliverable(&runtime, t)?
                            .filter(|delivery| delivery.stale)
                        {
                            // A contributor can complete a co-work revision after
                            // the writer has taken its immutable source snapshot.
                            // One final read of the now-settled sources is charged
                            // against the normal run and time budgets.
                            let sources_settled = delivery.sources.iter().all(|source| {
                                t.agents.iter().any(|a| {
                                    a.id == source.agent_id && a.status == "completed"
                                })
                            });
                            let owner = t.agents.iter_mut().find(|a| a.id == delivery.agent_id)
                                .context("Final delivery owner disappeared")?;
                            if sources_settled && !final_review {
                                owner.status = pending();
                                final_review = true;
                                Self::event(t, Some(&delivery.agent_id), "final_review",
                                    "Source evidence changed during publication; reviewing the latest completed snapshots");
                                return Ok(());
                            }
                            owner.status = "failed".into();
                            owner.error = Some(if sources_settled {
                                "Sources changed again during the final review. The last delivery is retained for inspection."
                            } else {
                                "A source still has unfinished work. Complete it before requesting another final review."
                            }.into());
                            complete = false;
                        }
                    }
                    t.status = if complete { "completed" } else { "failed" }.into();
                    crate::tunnel::close_all(
                        t,
                        "closed",
                        if complete {
                            "Workflow completed"
                        } else {
                            "Workflow stopped with an unfinished role"
                        },
                    );
                    let selected: HashSet<_> = t
                        .agents
                        .iter()
                        .filter(|agent| selected_for_run(t, agent))
                        .map(|agent| agent.id.clone())
                        .collect();
                    for a in &mut t.agents {
                        if a.status == "pending" && selected.contains(&a.id) {
                            a.status = "failed".into();
                            a.error = Some("A dependency did not complete".into());
                        }
                    }
                    Self::event(
                        t,
                        None,
                        "finished",
                        if complete {
                            "All agents completed"
                        } else {
                            "Stopped because a stage failed"
                        },
                    );
                    self.capture(t, "After run")
                })?;
                if matches!(terminal.status.as_str(), "running" | "cancelling") {
                    continue;
                }
                return Ok(terminal);
            }
            let wave_started = Instant::now();
            let mut dispatch = Vec::new();
            let mut expired = Vec::new();
            for turn in ready {
                let agent = &turn.agent;
                let deadline = team.execution_budget.as_ref().map(|budget| {
                    if turn.cowork_only {
                        started + Duration::from_secs(budget.max_runtime_secs)
                    } else {
                        budget.role_deadline(
                            started,
                            wave_started,
                            agent,
                            &mut publication_deadline,
                        )
                    }
                });
                if deadline.is_some_and(|limit| wave_started >= limit) {
                    expired.push(turn.agent.id);
                } else {
                    dispatch.push((turn, deadline));
                }
            }
            if !expired.is_empty() {
                self.mutate(id, |t| {
                    for agent_id in &expired {
                        if let Some(agent) = t.agents.iter_mut().find(|a| &a.id == agent_id) {
                            agent.status = "cancelled".into();
                            agent.error = Some("Admin execution time budget exhausted".into());
                        }
                        Self::event(
                            t,
                            Some(agent_id),
                            "agent_error",
                            "Admin execution time budget exhausted",
                        );
                    }
                    Ok(())
                })?;
            }
            if let Some(budget) = &team.execution_budget {
                let available = budget.max_agent_runs.saturating_sub(runs);
                ensure!(
                    dispatch.is_empty() || available > 0 || !running.is_empty(),
                    "Admin agent-run budget exhausted; send a new instruction to continue"
                );
                dispatch.truncate(available);
            }
            runs += dispatch.len();
            for (turn, deadline) in dispatch {
                let worker_id = turn.agent.id.clone();
                let publishing = !turn.cowork_only
                    && turn.agent.delivery == Some(crate::synthesis::DeliveryKind::Final);
                publication_ran |= publishing;
                let review = publishing && final_review;
                in_flight.insert(worker_id.clone());
                let store = self.clone();
                let runtime = runtime.clone();
                let id = id.to_owned();
                running.spawn(async move {
                    (
                        worker_id,
                        store.run_agent(runtime, &id, turn, deadline, review).await,
                    )
                });
            }
            if !running.is_empty() {
                tokio::select! {
                    Some(joined) = running.join_next() => {
                        let (worker_id, result) = joined?;
                        in_flight.remove(&worker_id);
                        if let Some(run) = self.active_runs.lock().get_mut(id) {
                            run.handles.remove(&worker_id);
                        }
                        result?;
                    }
                    _ = wakeup.notified() => {}
                }
            }
        }
    }
    async fn run_agent(
        self: &Arc<Self>,
        runtime: Arc<Runtime>,
        id: &str,
        turn: LaneTurn,
        deadline: Option<std::time::Instant>,
        final_review: bool,
    ) -> Result<()> {
        let LaneTurn { agent, cowork_only } = turn;
        let mut cowork_starts = Vec::new();
        let mut invitations = Vec::new();
        let team = self.mutate(id, |t| {
            if cowork_only {
                invitations = crate::tunnel::begin_side_turn(t, &agent.id);
                // The sender may settle/close the invitation just before admission.
                if invitations.is_empty() {
                    return Ok(());
                }
            }
            let current = t
                .agents
                .iter_mut()
                .find(|a| a.id == agent.id)
                .context("agent missing")?;
            current.status = "running".into();
            let at = Utc::now();
            t.events.push(TeamEvent {
                at: at.to_rfc3339(),
                agent_id: Some(agent.id.clone()),
                kind: "agent_started".into(),
                message: agent.name.clone(),
            });
            cowork_starts = crate::tunnel::mark_started(t, &agent.id, at, cowork_only);
            Ok(())
        })?;
        if cowork_only && invitations.is_empty() {
            return Ok(());
        }
        for (message_id, elapsed_ms) in cowork_starts {
            runtime
                .learn
                .observe_cowork_start(&message_id, &agent.profile_id, elapsed_ms);
        }
        let outcome: Result<()> = async {
            let kei = crate::kei::load_kei_config(&runtime.outer_home, &runtime.config.kei)?;
            let profile = kei.profile(&agent.profile_id)?;
            let mut runner = AgentLoop::with_profile(runtime.clone(), profile)?.with_cowork_only(cowork_only);
            if let Some(budget) = &team.execution_budget {
                runner = runner.with_execution_limits(
                    budget.max_steps_per_agent,
                    budget.max_tokens_per_step,
                    budget.max_tool_calls_per_agent,
                );
            }
            let session = runtime.sessions.get_or_load(&agent.session_id)?;
            let prepared = if cowork_only { None } else {
                Some(crate::synthesis::prepare(&runtime, &team, &agent, agent_prompt(&team, &agent))?)
            };
            let mut prompt = match &prepared {
                Some(delivery) => delivery.prompt.clone(),
                None => format!("You are {} (worker ID {}). Your main assignment is waiting for its dependencies. This is a separate, bounded co-work turn in your own lane. Work only on the incoming invitations {} in the attached peer transcript. Accept and carry out the requested check with your tools; complete_work using the invitation ID, or decline_work if appropriate. If information is missing, ask the requester a concrete question with resume_on_reply=true, then yield without polling. Preserve existing files, write only to your owned paths ({}) if the invitation requires it. Your main assignment will run separately after its inputs arrive. Do not publish or complete the primary assignment during this turn.", agent.name, agent.id, invitations.join(", "), agent.files.join(", ")),
            };
            if final_review {
                prompt.push_str("\nFinal publication review: a source completed or changed after the preceding draft was prepared. Read the current source deliveries and prior draft, apply any material differences, and confirm the final publication. Preserve accurate existing files. Send no additional peer requests for this closing check.");
            }
            let task = self.prepare_task(&runtime, &team, &agent, &prompt)?;
            let (tx, mut rx) = mpsc::channel(256);
            let handle = runner
                .run_team_task(task.id.clone(), session, prompt, tx)
                .await?;
            {
                self.active_runs
                    .lock()
                    .get_mut(id)
                    .context("team coordinator missing")?
                    .handles
                    .insert(agent.id.clone(), handle.clone());
            }
            let mut error = None;
            match self.read(id) {
                Ok(team) if team.status == "cancelling" => handle.cancel(),
                Err(failure) => {
                    error = Some(failure.to_string());
                    handle.cancel();
                }
                _ => {}
            }
            let mut output = String::new();
            let mut clock = tokio::time::interval(std::time::Duration::from_millis(100));
            loop {
                let event = tokio::select! {
                    event = rx.recv() => match event { Some(value) => value, None => break },
                    _ = clock.tick() => {
                        if deadline.is_some_and(|limit| std::time::Instant::now() >= limit) {
                            error = Some("Admin execution time budget exhausted".into());
                            handle.cancel();
                        }
                        match self.read(id) {
                            Ok(team) if team.status == "cancelling" => handle.cancel(),
                            Err(failure) => { error = Some(failure.to_string()); handle.cancel(); },
                            _ => {}
                        }
                        continue;
                    }
                };
                match event {
                    AgentEvent::TurnStarted(_) => {
                        let run_id = runtime
                            .tasks
                            .latest_run_for_task(&task.id)
                            .map(|run| run.id);
                        if let Err(failure) = self.mutate(id, |team| {
                            let current = team
                                .agents
                                .iter_mut()
                                .find(|candidate| candidate.id == agent.id)
                                .context("subagent missing")?;
                            current.run_id = run_id;
                            Ok(())
                        }) {
                            error = Some(failure.to_string());
                            handle.cancel();
                        }
                    }
                    AgentEvent::TextDelta(text) => {
                        output.push_str(&text);
                        self.live_outputs
                            .lock()
                            .entry((id.into(), agent.id.clone()))
                            .or_default()
                            .push_str(&text);
                    }
                    AgentEvent::Error(message) => {
                        error = Some(message);
                    }
                    AgentEvent::ToolStarted { name, .. } => {
                        if let Err(failure) = self.mutate(id, |t| {
                            Self::event(t, Some(&agent.id), "tool", &name);
                            Ok(())
                        }) {
                            error = Some(failure.to_string());
                            handle.cancel();
                        }
                    }
                    AgentEvent::ToolFinished { name, ok, .. } => {
                        if let Err(failure) = self.mutate(id, |t| {
                            Self::event(
                                t,
                                Some(&agent.id),
                                "tool_finished",
                                &format!("{name}: {}", if ok { "completed" } else { "failed" }),
                            );
                            Ok(())
                        }) {
                            error = Some(failure.to_string());
                            handle.cancel();
                        }
                    }
                    AgentEvent::ApprovalNeeded { name, summary, .. } => {
                        if let Err(failure) = self.mutate(id, |t| {
                            Self::event(
                                t,
                                Some(&agent.id),
                                "approval_needed",
                                &format!("{name}: {summary}"),
                            );
                            Ok(())
                        }) {
                            error = Some(failure.to_string());
                            handle.cancel();
                        }
                    }
                    AgentEvent::Done => break,
                    _ => {}
                }
            }
            let mut native_task = runtime
                .tasks
                .task(&task.id)
                .context("subagent task missing")?;
            if matches!(
                native_task.state,
                TaskState::Running | TaskState::WaitingApproval | TaskState::WaitingEvent
            ) {
                // The loop has ended. An infrastructure error (for example a
                // failed coordination/session write) must not leave its real
                // native lease looking live after the team role has failed.
                runtime.tasks.interrupt_team_task(&task.id)?;
                native_task = runtime
                    .tasks
                    .task(&task.id)
                    .context("subagent task missing")?;
                error.get_or_insert_with(|| {
                    "Native subagent ended before completing its task".into()
                });
            }
            let latest_run = runtime
                .tasks
                .latest_run_for_task(&task.id)
                .map(|run| run.id);
            if native_task.state == TaskState::Failed && error.is_none() {
                error = Some("Native subagent task failed".into());
            }
            if native_task.state == TaskState::Completed
                && error.is_none()
                && self.read(id)?.status != "cancelling"
            {
                if let Some(prepared) = &prepared { crate::synthesis::capture(
                    &runtime,
                    &self.read(id)?,
                    &agent,
                    &task.id,
                    latest_run.as_deref(),
                    prepared,
                )?; }
            }
            self.mutate(id, |t| {
                let cancelling = t.status == "cancelling";
                if cowork_only {
                    crate::tunnel::finish_side_turn(t, &agent.id, &invitations, error.as_deref());
                }
                let current = t
                    .agents
                    .iter_mut()
                    .find(|a| a.id == agent.id)
                    .context("agent missing")?;
                if !cowork_only { current.output = output; }
                current.run_id = latest_run;
                current.status = if cancelling {
                    "cancelled"
                } else if cowork_only {
                    "pending"
                } else if native_task.state == TaskState::Cancelled {
                    "cancelled"
                } else if error.is_some() {
                    "failed"
                } else if native_task.state != TaskState::Completed {
                    "failed"
                } else {
                    "completed"
                }
                .into();
                current.error = if cowork_only { None } else { error };
                let message = format!("{}: {}", current.name, current.status);
                Self::event(t, Some(&agent.id), "agent_finished", &message);
                Ok(())
            })?;
            self.live_outputs
                .lock()
                .remove(&(id.into(), agent.id.clone()));
            Ok(())
        }
        .await;
        if let Err(error) = outcome {
            self.live_outputs
                .lock()
                .remove(&(id.into(), agent.id.clone()));
            self.mutate(id, |t| {
                let cancelling = t.status == "cancelling";
                if cowork_only {
                    crate::tunnel::finish_side_turn(
                        t,
                        &agent.id,
                        &invitations,
                        Some(&error.to_string()),
                    );
                }
                let a = t
                    .agents
                    .iter_mut()
                    .find(|a| a.id == agent.id)
                    .context("agent missing")?;
                a.status = if cancelling {
                    "cancelled"
                } else if cowork_only {
                    "pending"
                } else {
                    "failed"
                }
                .into();
                a.error = (!cowork_only).then(|| error.to_string());
                if let Some(task) = a.task_id.as_deref().and_then(|id| runtime.tasks.task(id)) {
                    if !matches!(
                        task.state,
                        TaskState::Completed | TaskState::Cancelled | TaskState::Failed
                    ) {
                        runtime.tasks.transition_task(
                            &task.id,
                            if cancelling {
                                TaskState::Cancelled
                            } else {
                                TaskState::Failed
                            },
                        )?;
                    }
                    a.run_id = runtime
                        .tasks
                        .latest_run_for_task(&task.id)
                        .map(|run| run.id);
                }
                Self::event(t, Some(&agent.id), "agent_error", &error.to_string());
                Ok(())
            })?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "harness_tests.rs"]
mod native_tests;

fn default_cascade() -> bool {
    true
}

fn apply_queued_messages(team: &mut Team) {
    let queued: Vec<_> = team
        .messages
        .iter()
        .filter(|message| message.queued)
        .map(|message| (message.agent_id.clone(), message.cascade))
        .collect();
    if queued.is_empty() {
        return;
    }
    let mut affected: HashSet<String> = if queued.iter().any(|(target, _)| target.is_none()) {
        team.agents.iter().map(|agent| agent.id.clone()).collect()
    } else {
        queued
            .iter()
            .filter_map(|(target, _)| target.clone())
            .collect()
    };
    let mut cascade: HashSet<String> = queued
        .iter()
        .filter(|(_, propagate)| *propagate)
        .filter_map(|(target, _)| target.clone())
        .collect();
    loop {
        let before = cascade.len();
        for agent in &team.agents {
            if agent.depends_on.iter().any(|id| cascade.contains(id)) {
                cascade.insert(agent.id.clone());
                affected.insert(agent.id.clone());
            }
        }
        if before == cascade.len() {
            break;
        }
    }
    for agent in &mut team.agents {
        if affected.contains(&agent.id) {
            agent.status = pending();
            agent.error = None;
            agent.output.clear();
        }
    }
    if !matches!(team.status.as_str(), "running" | "cancelling") {
        team.dispatch_agent_ids = Some(affected.iter().cloned().collect());
    } else if let Some(scope) = &mut team.dispatch_agent_ids {
        for id in &affected {
            if !scope.contains(id) {
                scope.push(id.clone());
            }
        }
    }
    for message in &mut team.messages {
        message.queued = false;
    }
    TeamStore::event(
        team,
        None,
        "messages_applied",
        "Queued instructions applied at the next execution boundary",
    );
}

pub(crate) const PEER_COORDINATION_GUIDANCE: &str = r#"Use agent_tunnel with the exact peer ID. Choose the interaction by what the recipient needs to do:
- To share an observation, credit a contribution or say “已采纳”, use open/send as a notice. Both lanes show it. An acknowledgment needs no reply.
- Use ask for a specific unanswered question. Answer with send and reply_to. Use propose/accept for a decision requiring both participants' agreement.
- Use invite for co-work: name a bounded piece of the assignment a peer can actually perform and the result you need. The recipient uses accept_work or decline_work with reply_to pointing to the invite. After accepting, do the work in your own session using your tools and owned files. Preserve your original contribution when you report the added work. Use complete_work against that same invite with the result and any file references. An invitation or acceptance alone is not a delivery.
If your own deliverable still needs the answer or work result, set resume_on_reply=true on the request. Otherwise the result is available to the final author in the shared record. Continue available work without polling or requesting courtesy acknowledgments."#;

fn agent_prompt(team: &Team, agent: &TeamAgent) -> String {
    let inputs: Vec<_> = team
        .agents
        .iter()
        .filter(|a| agent.depends_on.contains(&a.id))
        .map(|a| format!("{} ({})\n{}", a.name, a.profile_id, a.output))
        .collect();
    let handoffs: Vec<_> = team
        .handoffs
        .iter()
        .filter(|h| h.to == agent.id)
        .map(|h| format!("From {}: {}\n{}", h.from, h.instruction, h.summary))
        .collect();
    let mut messages: Vec<_> = team
        .messages
        .iter()
        .rev()
        .filter(|m| {
            !m.queued
                && m.agent_id
                    .as_deref()
                    .is_none_or(|target| target == agent.id)
        })
        .map(|m| {
            format!(
                "{}: {}",
                if m.source.as_deref() == Some("admin") {
                    "ARIS admin assignment"
                } else if m.source.is_none() && team.admin.is_some() {
                    "Task instruction (source not recorded)"
                } else {
                    "User"
                },
                m.text
            )
        })
        .collect();
    messages.reverse();
    let peers = team
        .agents
        .iter()
        .filter(|peer| peer.id != agent.id && peer.subagent_id.is_some())
        .map(|peer| format!("{}: {} ({})", peer.id, peer.name, peer.role))
        .collect::<Vec<_>>()
        .join("; ");
    messages.push(format!(
        "Internal coordination: your worker id is {}. Available peer ids: {}.\n{}",
        agent.id, peers, PEER_COORDINATION_GUIDANCE
    ));
    format!("You are {} in ARIS, a general-purpose multi-agent harness. Role: {}.\nShared goal: {}\nYour assignment: {}\nYour browser identity is your session; use your own browser tools.\nShared artifact files: {}\nYour owned output files: {}\nRead relevant existing files before editing. Keep edits within your assigned output files; other agents can be working concurrently. Use shared files directly, preserve previous contributions, and report exact delivered paths and unfinished work. If no output files were assigned, return your findings in the response.\nDependency results:\n{}\nExplicit handoffs:\n{}\nConversation instructions with their actual source, oldest to newest (latest takes precedence):\n{}", agent.name,agent.role,team.goal,agent.prompt,team.files.join(", "),agent.files.join(", "),inputs.join("\n\n"),handoffs.join("\n\n"),messages.join("\n\n"))
}

pub(crate) fn validate_graph(agents: &[TeamAgent]) -> Result<()> {
    let finals: Vec<_> = agents
        .iter()
        .filter(|agent| agent.delivery == Some(crate::synthesis::DeliveryKind::Final))
        .collect();
    ensure!(
        finals.len() <= 1,
        "A workflow can have at most one final delivery role"
    );
    if let Some(final_agent) = finals.first() {
        ensure!(
            !agents
                .iter()
                .any(|agent| agent.depends_on.contains(&final_agent.id)),
            "A final delivery role must be terminal; other roles cannot depend on it"
        );
    }
    let ids: HashSet<_> = agents.iter().map(|a| a.id.as_str()).collect();
    ensure!(
        ids.len() == agents.len() && !ids.contains(""),
        "agent ids must be unique and nonempty"
    );
    for agent in agents {
        for dep in &agent.depends_on {
            ensure!(
                ids.contains(dep.as_str()) && dep != &agent.id,
                "invalid dependency {dep}"
            );
        }
    }
    let mut resolved = HashSet::new();
    loop {
        let before = resolved.len();
        for agent in agents {
            if agent
                .depends_on
                .iter()
                .all(|d| resolved.contains(d.as_str()))
            {
                resolved.insert(agent.id.as_str());
            }
        }
        if resolved.len() == agents.len() {
            return Ok(());
        }
        if before == resolved.len() {
            bail!("agent dependency graph contains a cycle");
        }
    }
}
struct LaneTurn {
    agent: TeamAgent,
    cowork_only: bool,
}

#[cfg(test)]
fn ready_wave(team: &Team, in_flight: &HashSet<String>) -> Vec<LaneTurn> {
    ready_wave_with_policy(team, in_flight, Default::default())
}

fn ready_wave_with_policy(
    team: &Team,
    in_flight: &HashSet<String>,
    scheduling: crate::reviewer_policy::SchedulingPolicy,
) -> Vec<LaneTurn> {
    let complete: HashSet<_> = team
        .agents
        .iter()
        .filter(|a| a.status == "completed" && !in_flight.contains(&a.id))
        .map(|a| &a.id)
        .collect();
    // A spawned future can still be opening or closing its native task when
    // another lane sends a signal. Its slot and files stay reserved until join.
    let mut files: HashSet<_> = team
        .agents
        .iter()
        .filter(|a| in_flight.contains(&a.id))
        .flat_map(|a| a.files.iter().map(|file| file.to_lowercase()))
        .collect();
    let mut ready = vec![];
    let mut candidates: Vec<_> = team
        .agents
        .iter()
        .filter(|a| {
            a.status == "pending" && !in_flight.contains(&a.id) && selected_for_run(team, a)
        })
        .map(|a| (a, crate::tunnel::pending_invite(team, &a.id)))
        .collect();
    // A queued collaboration gets the next available slot before unrelated work.
    let ranks = crate::reviewer_policy::lane_ranks(team, scheduling);
    candidates.sort_by_key(|(agent, invite)| {
        let rank = if invite.is_none() {
            ranks.get(agent.id.as_str()).copied().unwrap_or_default()
        } else {
            0
        };
        (invite.is_none(), std::cmp::Reverse(rank))
    });
    for (agent, invite) in candidates {
        if in_flight.len() + ready.len()
            >= team
                .execution_budget
                .as_ref()
                .map_or(8, |b| b.max_parallel.min(8))
        {
            break;
        }
        // This lane already met its original prerequisites. Its current request
        // can come from a prerequisite that is itself now on a follow-up turn.
        let reserved = invite.is_some_and(|message| message.reserved_turn());
        let resumed_worker =
            agent.delivery != Some(crate::synthesis::DeliveryKind::Final) && reserved;
        let cowork_only = crate::tunnel::blocks_final(team, agent)
            || (!resumed_worker
                && !agent.depends_on.iter().all(|d| {
                    complete.contains(d)
                        || team
                            .dispatch_agent_ids
                            .as_ref()
                            .is_some_and(|scope| !scope.contains(d))
                }));
        if cowork_only
            && (invite.is_none()
                || (!reserved && !crate::tunnel::has_followup_budget(team, &agent.id)))
        {
            continue;
        }
        let claims: Vec<_> = agent.files.iter().map(|p| p.to_lowercase()).collect();
        if claims.iter().any(|p| files.contains(p)) {
            continue;
        }
        files.extend(claims);
        ready.push(LaneTurn {
            agent: agent.clone(),
            cowork_only,
        });
    }
    ready
}

pub(crate) fn selected_for_run(team: &Team, agent: &TeamAgent) -> bool {
    team.execution_budget
        .as_ref()
        .is_none_or(|b| b.agent_ids.contains(&agent.id))
        && team
            .dispatch_agent_ids
            .as_ref()
            .is_none_or(|ids| ids.contains(&agent.id))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn agent(id: &str, deps: &[&str], files: &[&str]) -> TeamAgent {
        serde_json::from_value(serde_json::json!({"id":id,"name":id,"profile_id":"test","depends_on":deps,"files":files})).unwrap()
    }
    fn team(agents: Vec<TeamAgent>) -> Team {
        Team {
            id: new_id(),
            name: "test".into(),
            goal: "research".into(),
            status: "idle".into(),
            agents,
            files: vec!["result.txt".into()],
            handoffs: vec![],
            messages: vec![],
            checkpoints: vec![],
            events: vec![],
            created_at: now(),
            admin: None,
            execution_budget: None,
            dispatch_agent_ids: None,
            tunnels: vec![],
            tunnel_turns: HashMap::new(),
        }
    }
    #[cfg(windows)]
    #[test]
    fn team_state_replace_waits_for_a_reader_and_preserves_the_prior_ledger_on_failure() {
        use std::os::windows::fs::OpenOptionsExt;

        let root = std::env::temp_dir().join(format!("aris-team-share-{}", new_id()));
        let store = TeamStore::open(root.join("teams"), root.clone()).unwrap();
        let mut team = team(vec![]);
        store.save(&team).unwrap();
        let dir = store.directory(&team.id).unwrap();
        let target = dir.join("team.json");
        let staged_files = || {
            fs::read_dir(&dir)
                .unwrap()
                .flatten()
                .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "tmp"))
                .count()
        };
        let reader = || {
            OpenOptions::new()
                .read(true)
                .share_mode(0x1 | 0x2)
                .open(&target)
                .unwrap()
        };

        let blocker = reader(); // FILE_SHARE_READ | FILE_SHARE_WRITE, no FILE_SHARE_DELETE.
        assert!(team_replace_conflict(
            &std::io::Error::from_raw_os_error(5),
            &target
        ));
        TeamStore::event(
            &mut team,
            Some("editor"),
            "tool_finished",
            "read_file: completed",
        );
        std::thread::scope(|scope| {
            let update = scope.spawn(|| store.save(&team));
            let until = Instant::now() + Duration::from_secs(2);
            while staged_files() == 0 && Instant::now() < until && !update.is_finished() {
                std::thread::sleep(Duration::from_millis(2));
            }
            assert_eq!(staged_files(), 1);
            assert!(store.read(&team.id).unwrap().events.is_empty());
            std::thread::sleep(Duration::from_millis(40));
            drop(blocker);
            update.join().unwrap().unwrap();
        });
        let committed = fs::read(&target).unwrap();
        assert_eq!(store.read(&team.id).unwrap().events.len(), 1);
        assert_eq!(staged_files(), 0);

        // A reader that never moves must not remove the last good ledger or
        // strand the new one. The watchdog releases it on a broken retry loop.
        let blocker = reader();
        let (done, wait) = std::sync::mpsc::channel::<()>();
        let holder = std::thread::spawn(move || {
            let _ = wait.recv_timeout(Duration::from_secs(4));
            drop(blocker);
        });
        team.name = "not committed".into();
        let error = store.save(&team).unwrap_err();
        drop(done);
        holder.join().unwrap();
        assert!(error.to_string().contains(target.to_str().unwrap()));
        assert!(error
            .downcast_ref::<std::io::Error>()
            .unwrap()
            .raw_os_error()
            .is_some());
        assert_eq!(fs::read(&target).unwrap(), committed);
        assert_eq!(staged_files(), 0);

        let writable = fs::metadata(&target).unwrap().permissions();
        let mut readonly = writable.clone();
        readonly.set_readonly(true);
        fs::set_permissions(&target, readonly).unwrap();
        let denied = std::io::Error::from_raw_os_error(5);
        assert!(!team_replace_conflict(&denied, &target));
        let error = store.save(&team).unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<std::io::Error>()
                .unwrap()
                .raw_os_error(),
            Some(5)
        );
        assert_eq!(fs::read(&target).unwrap(), committed);
        assert_eq!(staged_files(), 0);
        fs::set_permissions(&target, writable).unwrap();
        fs::remove_dir_all(root).unwrap();
    }
    fn admin(session_id: &str) -> TeamAdmin {
        TeamAdmin {
            request_id: "capacity-request".into(),
            session_id: session_id.into(),
            profile_id: "test".into(),
            model_id: "local-capacity-fixture".into(),
            family: "gpt".into(),
            phase: "planning".into(),
            response: String::new(),
            error: None,
            requested_effort: None,
            applied_effort: None,
        }
    }
    #[test]
    fn publication_clock_is_bounded_and_isolated_from_other_workers() {
        let legacy = serde_json::json!({
            "agent_ids":["first","second","writer"], "max_agent_runs":11, "max_steps_per_agent":18,
            "max_tool_calls_per_agent":40, "max_context_chars":64000, "max_tokens_per_step":4096,
            "max_parallel":3, "max_runtime_secs":900
        });
        let mut budget: TeamExecutionBudget = serde_json::from_value(legacy).unwrap();
        let started = Instant::now();
        let at = |seconds| started + Duration::from_secs(seconds);
        let mut publication = None;
        let other = agent("second", &[], &[]);
        let mut writer = agent("writer", &["first", "second"], &[]);
        writer.delivery = Some(crate::synthesis::DeliveryKind::Final);
        assert_eq!(
            budget.role_deadline(started, at(899), &writer, &mut publication),
            at(900)
        );
        assert!(
            publication.is_none(),
            "an old budget keeps its shared deadline"
        );
        budget.finalization_window_secs = 480;
        assert_eq!(
            budget.role_deadline(started, at(900), &writer, &mut publication),
            at(900)
        );
        assert!(
            publication.is_none(),
            "eligibility cannot be granted after the research cutoff"
        );
        assert_eq!(
            budget.role_deadline(started, at(899), &writer, &mut publication),
            at(1379)
        );
        assert_eq!(
            budget.role_deadline(started, at(899), &other, &mut publication),
            at(900)
        );
        assert_eq!(
            budget.role_deadline(started, at(1200), &writer, &mut publication),
            at(1379)
        );
        assert_eq!(
            budget.role_deadline(started, at(1380), &writer, &mut publication),
            at(1379)
        );
        assert_eq!(
            budget.role_deadline(started, at(1200), &other, &mut publication),
            at(900)
        );
        assert_eq!(
            budget.role_deadline(started, at(100), &writer, &mut None),
            at(900)
        );
    }

    #[tokio::test]
    async fn admin_allocates_finalization_only_for_distinct_source_workers() {
        let fixture = crate::tests::RuntimeFixture::new(crate::SessionSettings::default(), false);
        let runtime = &fixture.runtime;
        for (upstream, delivery, expected) in [
            (vec![], Some(crate::synthesis::DeliveryKind::Final), 0),
            (
                vec!["source"],
                Some(crate::synthesis::DeliveryKind::Final),
                480,
            ),
            (vec!["source"], None, 0),
        ] {
            let mut source = agent("source", &[], &[]);
            source.status = "completed".into();
            let mut analysis = agent("analysis", &upstream, &[]);
            analysis.status = "completed".into();
            let mut writer = agent("writer", &["analysis"], &[]);
            writer.delivery = delivery;
            let mut original = team(vec![source, analysis, writer]);
            original.status = "planning".into();
            original.admin = Some(admin("controller-session"));
            runtime.teams.save(&original).unwrap();
            let result = runtime
                .teams
                .finish_admin(
                    runtime,
                    &original.id,
                    "capacity-request",
                    "Publish the existing research",
                    vec![],
                    vec![AdminAssignment {
                        agent_id: "writer".into(),
                        instruction: "Write the report".into(),
                    }],
                )
                .unwrap();
            assert_eq!(
                result.execution_budget.unwrap().finalization_window_secs,
                expected
            );
        }
    }

    #[test]
    fn admin_worker_capacity_counts_real_controller_and_saturates() {
        for count in [0, 1, 2, 7, 8, 9, 32] {
            let mut workers: Vec<_> = (0..count)
                .map(|index| {
                    let mut worker = agent(&format!("worker-{index}"), &[], &[]);
                    worker.session_id = format!("session-{index}");
                    worker
                })
                .collect();
            if let Some(first) = workers.first_mut() {
                // A manually named admin remains a worker, even without a native ID.
                first.id = "admin".into();
                first.role = "admin".into();
            }
            let mut t = team(workers);
            let capacity = ADMIN_WORKER_LIMIT.saturating_sub(count);
            assert_eq!(admin_worker_capacity(&t), capacity);
            t.admin = Some(admin("controller-session"));
            assert_eq!(admin_workers(&t).count(), count);
            assert_eq!(admin_worker_capacity(&t), capacity);
            let mut controller = agent("different-controller-name", &[], &[]);
            controller.session_id = "controller-session".into();
            t.agents.insert(0, controller);
            assert_eq!(admin_workers(&t).count(), count);
            assert_eq!(admin_worker_capacity(&t), capacity);
        }
        let mut legacy = team(vec![agent("admin", &[], &[])]);
        legacy.admin = Some(admin(""));
        assert_eq!(admin_workers(&legacy).count(), 1);
    }
    #[tokio::test]
    async fn admin_capacity_rejection_is_atomic_without_cli_validation() {
        let fixture = crate::tests::RuntimeFixture::new(crate::SessionSettings::default(), false);
        let runtime = &fixture.runtime;
        for (existing, added, assigned, expected_error) in
            [(7, 2, 2, "reach 8 workers"), (9, 0, 9, "at most 8 workers")]
        {
            let mut before = team(
                (0..existing)
                    .map(|index| agent(&format!("existing-{index}"), &[], &[]))
                    .collect(),
            );
            before.status = "planning".into();
            before.admin = Some(admin("controller-session"));
            runtime.teams.save(&before).unwrap();
            let session_count = runtime.sessions.list_ids().len();
            let additions = (0..added)
                .map(|index| agent(&format!("new-{index}"), &[], &[]))
                .collect();
            let assignments = (0..assigned)
                .map(|index| AdminAssignment {
                    agent_id: format!("{}-{index}", if added > 0 { "new" } else { "existing" }),
                    instruction: "Complete this assignment".into(),
                })
                .collect();

            // The core guard must reject before provider resolution or session creation.
            let error = runtime
                .teams
                .finish_admin(
                    runtime,
                    &before.id,
                    "capacity-request",
                    "Reject the oversized dispatch",
                    additions,
                    assignments,
                )
                .unwrap_err();
            assert!(error.to_string().contains(expected_error));
            assert_eq!(runtime.sessions.list_ids().len(), session_count);
            assert_eq!(
                serde_json::to_value(runtime.teams.read(&before.id).unwrap()).unwrap(),
                serde_json::to_value(before).unwrap()
            );
        }
    }
    #[test]
    fn admin_message_source_preserves_legacy_unknown_and_explicit_authors() {
        let old: TeamMessage = serde_json::from_value(serde_json::json!({
            "id":"message","agent_id":"a","text":"existing instruction","at":"2026-01-01T00:00:00Z"
        }))
        .unwrap();
        assert!(old.source.is_none());
        assert!(serde_json::to_value(&old).unwrap().get("source").is_none());
        for source in ["user", "admin"] {
            let mut message = old.clone();
            message.source = Some(source.into());
            let restored: TeamMessage =
                serde_json::from_value(serde_json::to_value(&message).unwrap()).unwrap();
            assert_eq!(restored.source.as_deref(), Some(source));
        }
    }
    #[test]
    fn workflow_policy_changes_live_eligible_lane_order_and_respects_files() {
        let mut t = team(vec![
            agent("a", &[], &["one"]),
            agent("b", &[], &[]),
            agent("c", &[], &["ONE"]),
            agent("d", &["c"], &[]),
        ]);
        t.execution_budget=Some(serde_json::from_value(serde_json::json!({
            "agent_ids":["a","b","c","d"],"max_parallel":2,"max_agent_runs":4,
            "max_steps_per_agent":12,"max_tool_calls_per_agent":32,"max_tokens_per_step":4096,"max_runtime_secs":900})).unwrap());
        let pick = |p| {
            ready_wave_with_policy(&t, &HashSet::new(), p)
                .into_iter()
                .map(|turn| turn.agent.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(pick(Default::default()), ["a", "b"]);
        assert_eq!(
            pick(crate::reviewer_policy::SchedulingPolicy::LongestDependencyChain),
            ["c", "b"]
        );
    }
    #[test]
    fn graph_and_shared_file_scheduling() {
        let agents = vec![
            agent("a", &[], &["report.md"]),
            agent("b", &[], &["REPORT.md"]),
            agent("c", &[], &["notes.md"]),
            agent("d", &["a", "b"], &[]),
        ];
        validate_graph(&agents).unwrap();
        assert_eq!(
            ready_wave(&team(agents), &HashSet::new())
                .iter()
                .map(|turn| turn.agent.id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "c"]
        );
        assert!(validate_graph(&[agent("a", &["b"], &[]), agent("b", &["a"], &[])]).is_err());
    }
    #[test]
    fn a_co_work_continuation_keeps_its_slot_and_file_constraints() {
        use serde_json::json;
        let mut t = team(vec![
            agent("sender", &[], &["budget.md"]),
            agent("other", &[], &[]),
            agent("helper", &["sender"], &["receipt.md"]),
        ]);
        t.execution_budget = Some(serde_json::from_value(json!({
            "agent_ids":["sender","other","helper"], "max_agent_runs":8, "max_steps_per_agent":12,
            "max_tool_calls_per_agent":24, "max_context_chars":64000, "max_tokens_per_step":4096,
            "max_parallel":2, "max_runtime_secs":120
        })).unwrap());
        t.tunnels.push(
            serde_json::from_value(json!({
                "id":"work","topic":"verify","status":"open","participants":["sender","helper"],
                "created_at":now(), "updated_at":now(), "messages":[{
                    "id":"invite","seq":1,"from":"sender","to":"helper","text":"check the budget",
                    "kind":"cowork_invite","expects_reply":true,"at":now(),
                    "session_id":"s","task_id":"t","turn_id":"turn","run_id":"r"
                }]
            }))
            .unwrap(),
        );
        t.agents[0].status = "running".into();
        let active = HashSet::from(["sender".to_owned()]);
        let selected = |team: &Team| {
            ready_wave(team, &active)
                .into_iter()
                .map(|turn| turn.agent.id)
                .collect::<Vec<_>>()
        };
        // An unstarted helper can answer the invite while its primary assignment
        // remains gated on the sender. A free slot does not bypass file leases.
        assert_eq!(selected(&t), ["helper"]);
        assert!(ready_wave(&t, &active)[0].cowork_only);
        // A helper may ask for missing inputs and yield. The requested answer
        // can resume the side turn while the primary prerequisite is still busy.
        let mut missing = t.clone();
        let original = missing.tunnels[0].messages[0].clone();
        let first = crate::tunnel::begin_side_turn(&mut missing, "helper");
        crate::tunnel::mark_started(&mut missing, "helper", Utc::now(), true);
        let mut question = original.clone();
        question.id = "need-quote".into();
        question.kind = "question".into();
        question.from = "helper".into();
        question.to = "sender".into();
        question.resume_on_reply = true;
        missing.tunnels[0].messages.push(question);
        crate::tunnel::finish_side_turn(&mut missing, "helper", &first, None);
        assert_eq!(selected(&missing), ["other"]);
        let mut answer = original;
        answer.id = "quote".into();
        answer.kind = "answer".into();
        answer.expects_reply = false;
        answer.reply_to = Some("need-quote".into());
        missing.tunnels[0].messages.push(answer);
        let followup = ready_wave(&missing, &active);
        assert_eq!(followup[0].agent.id, "helper");
        assert!(followup[0].cowork_only);
        assert_eq!(
            crate::tunnel::begin_side_turn(&mut missing, "helper"),
            first
        );
        assert!(crate::tunnel::begin_side_turn(&mut missing, "helper").is_empty());
        t.agents[2].status = "completed".into();
        crate::tunnel::schedule(&mut t);
        // An idle helper already satisfied the same dependency on its first
        // turn. The requester stays in flight while the helper has priority.
        assert_eq!(selected(&t), ["helper"]);
        assert!(!ready_wave(&t, &active)[0].cowork_only);
        t.agents[2].files = vec!["BUDGET.md".into()];
        assert_eq!(selected(&t), ["other"]);
        t.execution_budget.as_mut().unwrap().max_parallel = 1;
        assert!(selected(&t).is_empty());
    }

    #[test]
    fn graph_rejects_multiple_or_nonterminal_explicit_final_roles() {
        let mut first = agent("a", &[], &[]);
        first.delivery = Some(crate::synthesis::DeliveryKind::Final);
        let mut second = agent("b", &[], &[]);
        second.delivery = Some(crate::synthesis::DeliveryKind::Final);
        assert!(validate_graph(&[first.clone(), second])
            .unwrap_err()
            .to_string()
            .contains("at most one"));
        assert!(validate_graph(&[first, agent("b", &["a"], &[])])
            .unwrap_err()
            .to_string()
            .contains("terminal"));
        let supporting = agent("a", &[], &[]);
        let mut final_agent = agent("b", &["a"], &[]);
        final_agent.delivery = Some(crate::synthesis::DeliveryKind::Final);
        validate_graph(&[supporting, final_agent]).unwrap();
    }
    #[test]
    fn restart_bridges_native_terminal_commits_to_the_review_queue() {
        let fixture = crate::tests::RuntimeFixture::new(crate::SessionSettings::default(), true);
        let runtime = &fixture.runtime;
        let mut old = team(vec![]);
        old.status = "completed".into();
        TeamStore::event(
            &mut old,
            None,
            "finished",
            "Completed before workflow reviews existed",
        );
        runtime.teams.save(&old).unwrap();
        let mut source = team(vec![]);
        source.status = "running".into();
        runtime.teams.save(&source).unwrap();
        // The normal mutation persists an intent before it publishes the native terminal event.
        runtime
            .teams
            .mutate(&source.id, |team| {
                team.status = "completed".into();
                TeamStore::event(team, None, "finished", "All agents completed");
                Ok(())
            })
            .unwrap();
        let session = runtime.sessions.create();
        let sid = session.read().id.clone();
        session.write().append(crate::SessionEvent::TurnStart {
            id: "real-turn".into(),
            at: chrono::Utc::now(),
            profile_id: None,
            model_id: None,
            family: None,
        });
        runtime.sessions.persist_now_result(&session).unwrap();
        let mut task = TaskRecord::new(GoalSpec::new("chat run")).unwrap();
        task.session_id = Some(sid.clone());
        let task = runtime.tasks.create_task(task).unwrap();
        runtime
            .tasks
            .transition_task(&task.id, TaskState::Running)
            .unwrap();
        let run = runtime.tasks.create_run(&task.id, 1).unwrap();
        crate::review_intents::stage_turn(runtime, &task.id, &sid, &run.id, Some("real-turn"))
            .unwrap();
        runtime
            .tasks
            .claim_run(&run.id, "retired-worker", chrono::Utc::now())
            .unwrap();
        runtime
            .tasks
            .finish_run(
                &run.id,
                "retired-worker",
                TaskState::Completed,
                crate::RunState::Completed,
                chrono::Utc::now(),
            )
            .unwrap();
        // Neither top-level finalizer had a chance to run after these commits.
        assert!(!runtime.workspace_outer.join("workflow-reviews").exists());
        for _ in 0..2 {
            let reopened = Runtime::bootstrap(
                runtime.config.clone(),
                runtime.workspace_root.clone(),
                crate::DeepSeekClient::new(runtime.llm.config()).unwrap(),
                runtime.tools.clone(),
            )
            .unwrap();
            let all = crate::workflow_reviewer::get(&reopened, None, None).unwrap();
            assert_eq!(all["history"].as_array().unwrap().len(), 2);
            for (kind, id) in [("team", &source.id), ("chat", &sid)] {
                let rec = crate::workflow_reviewer::get(&reopened, Some(kind), Some(id)).unwrap();
                assert_eq!(rec["review"]["outcome"], "completed");
            }
            assert!(
                crate::workflow_reviewer::get(&reopened, Some("team"), Some(&old.id)).unwrap()
                    ["review"]
                    .is_null()
            );
            assert_eq!(
                std::fs::read_dir(reopened.workspace_outer.join("review-intents"))
                    .unwrap()
                    .count(),
                0
            );
        }
    }

    #[test]
    fn recovering_an_interrupted_coordinator_queues_one_review_for_the_team() {
        let fixture = crate::tests::RuntimeFixture::new(crate::SessionSettings::default(), true);
        let runtime = &fixture.runtime;
        let mut original = team(vec![agent("worker", &[], &[])]);
        original.status = "running".into();
        original.agents[0].status = "running".into();
        runtime.teams.save(&original).unwrap();
        for _ in 0..2 {
            let recovered = runtime.teams.get(&original.id).unwrap();
            assert_eq!(recovered.status, "failed");
        }
        let report = crate::workflow_reviewer::get(runtime, None, None).unwrap();
        assert_eq!(report["history"].as_array().unwrap().len(), 1);
        assert_eq!(report["review"]["subject_kind"], "team");
        assert_eq!(report["review"]["subject_id"], original.id);
        assert_eq!(report["review"]["outcome"], "failed");
    }

    #[test]
    fn chat_instructions_queue_without_interrupting_then_continue_only_target() {
        let root = std::env::temp_dir().join(format!("aris-queue-{}", new_id()));
        fs::create_dir_all(&root).unwrap();
        let store = TeamStore::open(root.join("teams"), root.clone()).unwrap();
        let mut t = team(vec![agent("a", &[], &[]), agent("b", &["a"], &[])]);
        t.status = "running".into();
        t.agents[0].status = "running".into();
        t.agents[1].status = "completed".into();
        t.agents[1].output = "existing result".into();
        store.save(&t).unwrap();
        let lease = store.run_lock(&t.id).unwrap();
        let mut queued = store
            .chat_message(&t.id, Some("a"), "Please explain the sources")
            .unwrap();
        assert_eq!(queued.status, "running");
        assert!(queued.messages[0].queued);
        assert_eq!(queued.agents[0].status, "running");
        assert!(!agent_prompt(&queued, &queued.agents[0]).contains("Please explain the sources"));
        apply_queued_messages(&mut queued);
        assert!(!queued.messages[0].queued);
        assert_eq!(queued.agents[0].status, "pending");
        assert_eq!(queued.agents[1].status, "completed");
        assert_eq!(queued.agents[1].output, "existing result");
        assert!(agent_prompt(&queued, &queued.agents[0]).contains("Please explain the sources"));
        for index in 0..9 {
            store
                .chat_message(&t.id, Some("a"), &format!("queued-note-{index}"))
                .unwrap();
        }
        let mut batch = store.read(&t.id).unwrap();
        apply_queued_messages(&mut batch);
        let prompt = agent_prompt(&batch, &batch.agents[0]);
        for index in 0..9 {
            assert!(prompt.contains(&format!("queued-note-{index}")));
        }
        drop(lease);
        store.save(&queued).unwrap();
        let mut cascaded = store
            .message(&t.id, Some("a"), "Revise the deliverable")
            .unwrap();
        apply_queued_messages(&mut cascaded);
        assert_eq!(cascaded.agents[1].status, "pending");
        drop(store);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn restore_snapshots_preserves_unmanaged_files_and_creates_undo() {
        let root = std::env::temp_dir().join(format!("aris-snapshots-{}", new_id()));
        fs::create_dir_all(&root).unwrap();
        let store = TeamStore::open(root.join(".aris/teams"), root.clone()).unwrap();
        let mut t = team(vec![agent("a", &[], &[])]);
        store.save(&t).unwrap();
        fs::write(root.join("unmanaged.txt"), "untouched").unwrap();
        t = store.checkpoint(&t.id, "empty").unwrap();
        let empty = t.checkpoints[0].id.clone();
        fs::write(root.join("result.txt"), "first").unwrap();
        t = store.checkpoint(&t.id, "first").unwrap();
        let first = t.checkpoints[1].id.clone();
        fs::write(root.join("result.txt"), "second").unwrap();
        t = store.restore(&t.id, &first).unwrap();
        assert_eq!(
            fs::read_to_string(root.join("result.txt")).unwrap(),
            "first"
        );
        assert_eq!(t.checkpoints.last().unwrap().label, "Before restore (undo)");
        store.restore(&t.id, &empty).unwrap();
        assert!(!root.join("result.txt").exists());
        assert_eq!(
            fs::read_to_string(root.join("unmanaged.txt")).unwrap(),
            "untouched"
        );
        assert!(store.artifact_path("../outside").is_err());
        assert!(store.artifact_path(".git/config").is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
