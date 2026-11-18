//! Durable multi-agent workspaces. File snapshots never touch the user's Git index.
use crate::{AgentEvent, AgentHandle, AgentLoop, Runtime};
use anyhow::{bail, ensure, Context, Result};
use chrono::Utc;
use fs2::FileExt;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, OpenOptions},
    path::{Component, Path, PathBuf},
    sync::Arc,
};
use tokio::sync::mpsc;
use uuid::Uuid;

fn now() -> String {
    Utc::now().to_rfc3339()
}
fn new_id() -> String {
    Uuid::new_v4().to_string()
}
fn pending() -> String {
    "pending".into()
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
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamSpec {
    pub name: String,
    pub goal: String,
    pub agents: Vec<TeamAgent>,
    #[serde(default)]
    pub files: Vec<String>,
}

pub struct TeamStore {
    root: PathBuf,
    workspace: PathBuf,
    transaction: Mutex<()>,
    handles: Mutex<HashMap<String, Vec<AgentHandle>>>,
    live_outputs: Mutex<HashMap<(String, String), String>>,
}

impl TeamStore {
    pub fn open(root: PathBuf, workspace: PathBuf) -> Result<Self> {
        fs::create_dir_all(&root)?;
        Ok(Self {
            root,
            workspace: workspace.canonicalize()?,
            transaction: Mutex::new(()),
            handles: Mutex::new(HashMap::new()),
            live_outputs: Mutex::new(HashMap::new()),
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
        fs::create_dir_all(&dir)?;
        let temp = dir.join(format!("{}.tmp", new_id()));
        fs::write(&temp, serde_json::to_vec_pretty(team)?)?;
        fs::rename(temp, dir.join("team.json"))?;
        Ok(())
    }
    fn run_lock(&self, id: &str) -> Result<File> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.directory(id)?.join("run.lock"))?;
        file.try_lock_exclusive()
            .context("team is already running in another process")?;
        Ok(file)
    }
    pub fn get(&self, id: &str) -> Result<Team> {
        let _guard = self.transaction.lock();
        let _state = self.state_lock(id)?;
        let mut team = self.read(id)?;
        if matches!(team.status.as_str(), "running" | "cancelling")
            && !self.handles.lock().contains_key(id)
        {
            if let Ok(_lease) = self.run_lock(id) {
                team.status = "failed".into();
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
                self.save(&team)?;
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
    fn mutate(&self, id: &str, update: impl FnOnce(&mut Team) -> Result<()>) -> Result<Team> {
        let _guard = self.transaction.lock();
        let _state = self.state_lock(id)?;
        let mut team = self.read(id)?;
        update(&mut team)?;
        self.save(&team)?;
        Ok(team)
    }
    fn event(team: &mut Team, agent_id: Option<&str>, kind: &str, message: &str) {
        team.events.push(TeamEvent {
            at: now(),
            agent_id: agent_id.map(str::to_owned),
            kind: kind.into(),
            message: message.into(),
        });
    }
    pub fn create(&self, runtime: &Runtime, mut spec: TeamSpec) -> Result<Team> {
        ensure!(
            !spec.name.trim().is_empty() && spec.name.len() <= 200,
            "team name is required (max 200 bytes)"
        );
        ensure!(
            !spec.goal.trim().is_empty() && spec.goal.len() <= 32000,
            "goal is required (max 32000 bytes)"
        );
        ensure!(
            !spec.agents.is_empty() && spec.agents.len() <= 32,
            "a team needs 1–32 agents"
        );
        validate_graph(&spec.agents)?;
        let kei = crate::kei::load_kei_config(&runtime.outer_home, &runtime.config.kei)?;
        for agent in &mut spec.agents {
            ensure!(
                !agent.name.trim().is_empty() && agent.name.len() <= 200,
                "agent name is required"
            );
            ensure!(
                agent.prompt.len() <= 32000 && agent.role.len() <= 4000,
                "agent instructions are too long"
            );
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
        };
        Self::event(&mut team, None, "created", "Shared workspace created");
        let _guard = self.transaction.lock();
        self.save(&team)?;
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
        ensure!(label.len() <= 200, "checkpoint label is too long");
        let id = new_id();
        let directory = self.directory(&team.id)?.join("snapshots").join(&id);
        fs::create_dir_all(&directory)?;
        let mut files = vec![];
        let mut total = 0u64;
        for (index, relative) in team.files.iter().enumerate() {
            let path = self.artifact_path(relative)?;
            let exists = path.try_exists()?;
            let size = if exists {
                fs::metadata(&path)?.len()
            } else {
                0
            };
            total += size;
            ensure!(
                total <= 64 * 1024 * 1024,
                "managed artifacts exceed 64 MiB per checkpoint"
            );
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
            ensure!(team.status == "running", "team is not running");
            team.status = "cancelling".into();
            Self::event(
                team,
                None,
                "cancelling",
                "Cancellation requested; waiting for active tools to stop",
            );
            Ok(())
        })?;
        if let Some(handles) = self.handles.lock().get(id) {
            for handle in handles {
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
            ensure!(from != to && instruction.len() <= 32000, "invalid handoff");
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
    fn submit_message(
        &self,
        id: &str,
        agent_id: Option<&str>,
        text: &str,
        cascade: bool,
    ) -> Result<Team> {
        ensure!(
            !text.trim().is_empty() && text.len() <= 32000,
            "message needs 1–32000 bytes"
        );
        self.mutate(id, |team| {
            if let Some(target) = agent_id {
                ensure!(
                    team.agents.iter().any(|a| a.id == target),
                    "agent not found"
                );
            }
            let queued = matches!(team.status.as_str(), "running" | "cancelling");
            team.messages.push(TeamMessage {
                id: new_id(),
                agent_id: agent_id.map(str::to_owned),
                text: text.trim().into(),
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
        let team = self.get(id)?;
        let lease = self.run_lock(id)?;
        let file_leases = self.artifact_leases(&team)?;
        let kei = crate::kei::load_kei_config(&runtime.outer_home, &runtime.config.kei)?;
        // Validate all connections before marking any stage running.
        ensure!(
            !matches!(team.status.as_str(), "running" | "cancelling"),
            "team is already running"
        );
        for agent in &team.agents {
            kei.profile(&agent.profile_id)?.resolve(
                &kei,
                &runtime.llm.config(),
                &runtime.outer_home,
            )?;
        }
        let team = self.mutate(id, |team| {
            apply_queued_messages(team);
            ensure!(
                team.agents.iter().any(|a| a.status != "completed"),
                "all agents completed; send a message to continue"
            );
            self.capture(team, "Before run")?;
            for agent in &mut team.agents {
                if agent.status != "completed" {
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
            Ok(())
        })?;
        self.handles.lock().insert(id.into(), vec![]);
        let store = self.clone();
        let id = id.to_owned();
        tokio::spawn(async move {
            let result = store.drive(runtime, &id).await;
            if let Err(error) = result {
                let _ = store.mutate(&id, |t| {
                    t.status = "failed".into();
                    Self::event(t, None, "error", &error.to_string());
                    Ok(())
                });
            }
            store.handles.lock().remove(&id);
            drop(file_leases);
            drop(lease);
        });
        Ok(team)
    }
    async fn drive(self: &Arc<Self>, runtime: Arc<Runtime>, id: &str) -> Result<()> {
        loop {
            let team = self.mutate(id, |t| {
                if t.status != "cancelling" {
                    apply_queued_messages(t);
                }
                Ok(())
            })?;
            if team.status == "cancelling" {
                self.mutate(id, |t| {
                    t.status = "cancelled".into();
                    for a in &mut t.agents {
                        if a.status == "pending" {
                            a.status = "cancelled".into();
                        }
                    }
                    Self::event(t, None, "cancelled", "All active agents stopped");
                    self.capture(t, "After cancellation")
                })?;
                return Ok(());
            }
            let ready = ready_wave(&team);
            if ready.is_empty() {
                self.mutate(id, |t| {
                    // A message may arrive after ready_wave was computed. Keep
                    // the run alive and consume it on the next safe boundary.
                    if t.status == "cancelling" || t.messages.iter().any(|message| message.queued) {
                        return Ok(());
                    }
                    let complete = t.agents.iter().all(|a| a.status == "completed");
                    t.status = if complete { "completed" } else { "failed" }.into();
                    for a in &mut t.agents {
                        if a.status == "pending" {
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
                if matches!(self.read(id)?.status.as_str(), "running" | "cancelling") {
                    continue;
                }
                return Ok(());
            }
            let mut running = tokio::task::JoinSet::new();
            for agent in ready {
                let store = self.clone();
                let runtime = runtime.clone();
                let id = id.to_owned();
                running.spawn(async move { store.run_agent(runtime, &id, agent).await });
            }
            let mut first_error = None;
            while let Some(result) = running.join_next().await {
                let error = match result {
                    Ok(Ok(())) => None,
                    Ok(Err(error)) => Some(error),
                    Err(error) => Some(error.into()),
                };
                if let Some(error) = error {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                    if let Some(handles) = self.handles.lock().get(id) {
                        for handle in handles {
                            handle.cancel();
                        }
                    }
                }
            }
            self.handles.lock().entry(id.into()).or_default().clear();
            if let Some(error) = first_error {
                return Err(error);
            }
        }
    }
    async fn run_agent(
        self: &Arc<Self>,
        runtime: Arc<Runtime>,
        id: &str,
        agent: TeamAgent,
    ) -> Result<()> {
        let team = self.mutate(id, |t| {
            let current = t
                .agents
                .iter_mut()
                .find(|a| a.id == agent.id)
                .context("agent missing")?;
            current.status = "running".into();
            Self::event(t, Some(&agent.id), "agent_started", &agent.name);
            Ok(())
        })?;
        let outcome: Result<()> = async {
            let kei = crate::kei::load_kei_config(&runtime.outer_home, &runtime.config.kei)?;
            let profile = kei.profile(&agent.profile_id)?;
            let runner = AgentLoop::with_profile(runtime.clone(), profile)?;
            let session = runtime.sessions.get_or_load(&agent.session_id)?;
            let prompt = agent_prompt(&team, &agent);
            let (tx, mut rx) = mpsc::channel(256);
            let handle = runner.run_turn(session, prompt, tx).await?;
            {
                self.handles
                    .lock()
                    .entry(id.into())
                    .or_default()
                    .push(handle.clone());
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
            loop {
                let event = tokio::select! {
                    event = rx.recv() => match event { Some(value) => value, None => break },
                    _ = tokio::time::sleep(std::time::Duration::from_millis(250)) => {
                        match self.read(id) {
                            Ok(team) if team.status == "cancelling" => handle.cancel(),
                            Err(failure) => { error = Some(failure.to_string()); handle.cancel(); },
                            _ => {}
                        }
                        continue;
                    }
                };
                match event {
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
            self.mutate(id, |t| {
                let cancelling = t.status == "cancelling";
                let current = t
                    .agents
                    .iter_mut()
                    .find(|a| a.id == agent.id)
                    .context("agent missing")?;
                current.output = output;
                current.status = if cancelling {
                    "cancelled"
                } else if error.is_some() {
                    "failed"
                } else {
                    "completed"
                }
                .into();
                current.error = error;
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
                let a = t
                    .agents
                    .iter_mut()
                    .find(|a| a.id == agent.id)
                    .context("agent missing")?;
                a.status = "failed".into();
                a.error = Some(error.to_string());
                Self::event(t, Some(&agent.id), "agent_error", &error.to_string());
                Ok(())
            })?;
        }
        Ok(())
    }
}

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
        .map(|m| m.text.as_str())
        .collect();
    messages.reverse();
    format!("You are {} in ARIS, a general-purpose multi-agent harness. Role: {}.\nShared goal: {}\nYour assignment: {}\nYour browser identity is your session; use your own browser tools.\nShared artifact files: {}\nYour owned output files: {}\nRead relevant existing files before editing. Keep edits within your assigned output files; other agents can be working concurrently. Use shared files directly, preserve previous contributions, and report exact delivered paths and unfinished work. If no output files were assigned, return your findings in the response.\nDependency results:\n{}\nExplicit handoffs:\n{}\nUser follow-up instructions, oldest to newest (latest takes precedence):\n{}", agent.name,agent.role,team.goal,agent.prompt,team.files.join(", "),agent.files.join(", "),inputs.join("\n\n"),handoffs.join("\n\n"),messages.join("\n\n"))
}

pub(crate) fn validate_graph(agents: &[TeamAgent]) -> Result<()> {
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
fn ready_wave(team: &Team) -> Vec<TeamAgent> {
    let complete: HashSet<_> = team
        .agents
        .iter()
        .filter(|a| a.status == "completed")
        .map(|a| &a.id)
        .collect();
    let mut files = HashSet::new();
    let mut ready = vec![];
    for agent in &team.agents {
        if ready.len() >= 8 {
            break;
        }
        if agent.status != "pending" || !agent.depends_on.iter().all(|d| complete.contains(d)) {
            continue;
        }
        let claims: Vec<_> = agent.files.iter().map(|p| p.to_lowercase()).collect();
        if claims.iter().any(|p| files.contains(p)) {
            continue;
        }
        files.extend(claims);
        ready.push(agent.clone());
    }
    ready
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
        }
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
            ready_wave(&team(agents))
                .iter()
                .map(|a| a.id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "c"]
        );
        assert!(validate_graph(&[agent("a", &["b"], &[]), agent("b", &["a"], &[])]).is_err());
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
