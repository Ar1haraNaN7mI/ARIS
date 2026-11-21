//! Durable deliverables and evidence for multi-step teams. A final publication
//! is explicitly designated by the plan, never inferred from role/file names.
use crate::{
    harness::{brief, Team, TeamAgent},
    Runtime, Session, SessionEvent, TaskState,
};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{BufReader, BufWriter, Read, Write},
    path::{Component, Path, PathBuf},
};

// Transport granularity for offset-based reads. There is no maximum page count.
const PAGE_CHARS: usize = 2048;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryKind {
    Final,
    Supporting,
    Independent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliveryArtifact {
    pub path: String,
    pub text: String,
    pub sha256: String,
    pub size_bytes: usize,
    pub event_id: String,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub reused: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliverySource {
    pub agent_id: String,
    pub session_id: String,
    pub task_id: Option<String>,
    pub run_id: Option<String>,
    pub turn_id: String,
    pub event_id: String,
    pub text: String,
    pub artifacts: Vec<DeliveryArtifact>,
    pub references: Vec<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Delivery {
    pub agent_id: String,
    pub session_id: String,
    /// These IDs identify the retained completed snapshot, including when a
    /// newer run is pending/failed. They never identify unfinished content.
    pub task_id: Option<String>,
    pub run_id: Option<String>,
    pub status: String,
    pub text: String,
    pub artifacts: Vec<DeliveryArtifact>,
    pub sources: Vec<DeliverySource>,
    pub tunnel_ids: Vec<String>,
    pub stale: bool,
    pub snapshot_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Snapshot {
    team_id: String,
    owner: DeliverySource,
    sources: Vec<DeliverySource>,
    tunnels: Vec<Value>,
    at: String,
}

pub struct PreparedDelivery {
    pub prompt: String,
    sources: Vec<DeliverySource>,
    tunnels: Vec<Value>,
    before_files: HashMap<String, Option<String>>,
    exclusive_files: HashSet<String>,
    prior_artifacts: Vec<DeliveryArtifact>,
}

/// Initialized once before the worker's first model call. This captures the
/// pre-turn file state without reloading team metadata at each stop candidate.
pub struct DeliveryCheck {
    workspace: PathBuf,
    agent: TeamAgent,
    proof: PreparedDelivery,
}

impl DeliveryCheck {
    pub fn report_rewrite_hint(&self) -> Option<&'static str> {
        self.agent.files.iter().any(|path| is_final_report(&self.agent, path)).then_some(
            "For missing Markdown reports, write reader-facing prose in the user's language. Turn the peer facts into conclusions and retain the figures, units and sources. Use the requested formats for other files.",
        )
    }

    pub fn missing(&self, session: &Session) -> Result<Vec<String>> {
        ensure!(
            session.id == self.agent.session_id,
            "Delivery check belongs to another session"
        );
        let source = source_with_proof(
            &self.workspace,
            &self.agent,
            session,
            None,
            None,
            Some(&self.proof),
            false,
            false,
        )?;
        Ok(self
            .agent
            .files
            .iter()
            .filter(|path| {
                is_text_output(path)
                    && !source
                        .artifacts
                        .iter()
                        .any(|artifact| artifact.path == **path)
            })
            .cloned()
            .collect())
    }
}

pub fn delivery_check(runtime: &Runtime, task_id: &str) -> Result<Option<DeliveryCheck>> {
    let Some(task) = runtime.tasks.task(task_id) else {
        return Ok(None);
    };
    let Some(owner) = task.owner else {
        return Ok(None);
    };
    let team = runtime.teams.get(&owner.team_id)?;
    let agent = team
        .agents
        .iter()
        .find(|agent| agent.id == owner.agent_id)
        .context("Delivery worker is missing")?;
    if agent.delivery.is_none() || !agent.files.iter().any(|path| is_text_output(path)) {
        return Ok(None);
    }
    ensure!(
        task.session_id.as_deref() == Some(agent.session_id.as_str()),
        "Delivery task does not own this session"
    );
    Ok(Some(DeliveryCheck {
        workspace: runtime.workspace_root.clone(),
        agent: agent.clone(),
        proof: prepare_files(runtime, &team, agent)?,
    }))
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn file_digest(path: &Path) -> Result<String> {
    let mut digest = Sha256::new();
    let mut file = fs::File::open(path)?;
    let mut block = [0u8; 32768];
    loop {
        let count = file.read(&mut block)?;
        if count == 0 {
            break;
        }
        digest.update(&block[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn directory(runtime: &Runtime, team: &Team) -> Result<PathBuf> {
    uuid::Uuid::parse_str(&team.id).context("invalid delivery team id")?;
    Ok(runtime
        .workspace_outer
        .join("teams")
        .join(&team.id)
        .join("deliveries"))
}

fn load(runtime: &Runtime, team: &Team, agent: &TeamAgent) -> Result<Option<Snapshot>> {
    let path = directory(runtime, team)?.join(format!("latest-{}.json", hash(agent.id.as_bytes())));
    if !path.try_exists()? {
        return Ok(None);
    }
    let snapshot: Snapshot = serde_json::from_reader(BufReader::new(fs::File::open(path)?))?;
    ensure!(
        snapshot.team_id == team.id
            && snapshot.owner.agent_id == agent.id
            && snapshot.owner.session_id == agent.session_id,
        "Delivery snapshot identity mismatch"
    );
    Ok(Some(snapshot))
}

fn save(runtime: &Runtime, team: &Team, snapshot: &Snapshot) -> Result<()> {
    let root = directory(runtime, team)?;
    fs::create_dir_all(&root)?;
    let immutable = root.join(format!(
        "run-{}.json",
        hash(format!("{}\0{}", snapshot.owner.agent_id, snapshot.owner.turn_id).as_bytes())
    ));
    let latest = root.join(format!(
        "latest-{}.json",
        hash(snapshot.owner.agent_id.as_bytes())
    ));
    for target in [immutable, latest] {
        let temporary = target.with_extension(format!("json.tmp.{}", uuid::Uuid::new_v4()));
        let publish = (|| -> Result<()> {
            let mut out = BufWriter::new(fs::File::create_new(&temporary)?);
            serde_json::to_writer(&mut out, snapshot)?;
            out.flush()?;
            drop(out);
            fs::rename(&temporary, &target)?;
            Ok(())
        })();
        if let Err(error) = publish {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
    }
    Ok(())
}

fn artifact_path(workspace: &Path, declared: &str) -> Result<PathBuf> {
    let relative = Path::new(declared);
    ensure!(
        !relative.as_os_str().is_empty()
            && !relative.is_absolute()
            && relative
                .components()
                .all(|part| matches!(part, Component::Normal(_))),
        "Invalid deliverable file path"
    );
    let root = workspace.canonicalize()?;
    let mut current = root.clone();
    for component in relative.components() {
        let name = component.as_os_str().to_string_lossy().to_ascii_lowercase();
        ensure!(
            !matches!(name.as_str(), ".aris" | ".git" | "credentials.env")
                && !name.starts_with(".env"),
            "Private files cannot be published as deliverables"
        );
        current.push(component);
        if let Ok(meta) = fs::symlink_metadata(&current) {
            ensure!(
                !meta.file_type().is_symlink(),
                "Deliverable files cannot use symlinks"
            );
        }
    }
    let canonical = current.canonicalize()?;
    ensure!(
        canonical.starts_with(root) && canonical.is_file(),
        "Deliverable file is outside the workspace"
    );
    Ok(canonical)
}

fn called_paths(name: &str, arguments: &Value) -> Vec<String> {
    match name {
        "write_file" | "edit_file" | "create_file" => arguments
            .get("path")
            .or_else(|| arguments.get("file_path"))
            .and_then(Value::as_str)
            .map(|path| vec![path.into()])
            .unwrap_or_default(),
        "apply_patch" => arguments
            .get("patch")
            .and_then(Value::as_str)
            .into_iter()
            .flat_map(|patch| patch.lines())
            .filter_map(|line| {
                line.strip_prefix("*** Update File:")
                    .or_else(|| line.strip_prefix("*** Add File:"))
                    .map(|path| path.trim().into())
            })
            .collect(),
        _ => vec![],
    }
}

fn completed_source(
    workspace: &Path,
    agent: &TeamAgent,
    session: &Session,
    task_id: Option<String>,
    run_id: Option<String>,
) -> Result<DeliverySource> {
    completed_source_with_proof(workspace, agent, session, task_id, run_id, None)
}

fn is_text_output(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "md" | "markdown"
                    | "mdx"
                    | "txt"
                    | "json"
                    | "csv"
                    | "tsv"
                    | "html"
                    | "htm"
                    | "xml"
                    | "yaml"
                    | "yml"
                    | "rst"
                    | "tex"
            )
        })
}

fn is_final_report(agent: &TeamAgent, path: &str) -> bool {
    if agent.delivery != Some(DeliveryKind::Final) {
        return false;
    }
    let path = Path::new(path);
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("md" | "markdown") => true,
        Some("txt") => path
            .file_stem()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                name.to_ascii_lowercase().contains("report") || name.contains("报告")
            }),
        _ => false,
    }
}

/// A report may quote code or contain a JSON example. Inspect only a whole,
/// well-formed ARIS packet (optionally in a single JSON fence).
fn is_internal_report_packet(body: &str) -> bool {
    let body = body.trim().trim_start_matches('\u{feff}').trim();
    let body = if let Some(fenced) = body.strip_prefix("```") {
        let Some((lang, rest)) = fenced.split_once('\n') else {
            return false;
        };
        if !lang.trim().is_empty() && !lang.trim().eq_ignore_ascii_case("json") {
            return false;
        }
        let Some((contents, closing)) = rest.rsplit_once('\n') else {
            return false;
        };
        if closing.trim() != "```" {
            return false;
        }
        contents.trim()
    } else {
        body
    };
    if !body.starts_with('{') {
        return false;
    }
    let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(body) else {
        return false;
    };
    if obj.get("type").and_then(Value::as_str) == Some("aris.brief")
        || (obj.len() == 1 && obj.get("peer_inbox").is_some_and(Value::is_array))
    {
        return true;
    }
    if obj.get("blocking") == Some(&Value::Bool(false))
        && obj
            .keys()
            .all(|k| matches!(k.as_str(), "blocking" | "tunnel" | "tunnels" | "notice"))
    {
        let channel = |c: &Value| c["id"].is_string() && c["messages"].is_array();
        if obj.get("tunnel").is_some_and(channel)
            || obj
                .get("tunnels")
                .and_then(Value::as_array)
                .is_some_and(|channels| channels.iter().all(channel))
        {
            return true;
        }
    }
    if obj.get("name").and_then(Value::as_str) == Some("agent_tunnel")
        && obj.contains_key("arguments")
    {
        return true;
    }
    // Bare function arguments require an ARIS action and a valid route.
    let action = obj.get("action").and_then(Value::as_str);
    matches!(
        action,
        Some(
            "open"
                | "send"
                | "ask"
                | "invite"
                | "propose"
                | "accept"
                | "accept_work"
                | "decline_work"
                | "complete_work"
                | "close"
                | "read"
        )
    ) && (obj.get("tunnel_id").is_some_and(Value::is_string)
        || (matches!(action, Some("open" | "send" | "ask" | "invite"))
            && obj.get("to").is_some_and(Value::is_string)
            && obj.get("topic").is_some_and(Value::is_string)))
        && obj.keys().all(|key| {
            matches!(
                key.as_str(),
                "action"
                    | "tunnel_id"
                    | "to"
                    | "topic"
                    | "text"
                    | "data"
                    | "reply_to"
                    | "proposal_id"
                    | "expects_reply"
                    | "resume_on_reply"
            )
        })
}

fn completed_source_with_proof(
    workspace: &Path,
    agent: &TeamAgent,
    session: &Session,
    task_id: Option<String>,
    run_id: Option<String>,
    proof: Option<&PreparedDelivery>,
) -> Result<DeliverySource> {
    source_with_proof(
        workspace, agent, session, task_id, run_id, proof, true, true,
    )
}

#[allow(clippy::too_many_arguments)]
fn source_with_proof(
    workspace: &Path,
    agent: &TeamAgent,
    session: &Session,
    task_id: Option<String>,
    run_id: Option<String>,
    proof: Option<&PreparedDelivery>,
    require_end: bool,
    require_complete: bool,
) -> Result<DeliverySource> {
    let start = session
        .events
        .iter()
        .rposition(|event| matches!(event, SessionEvent::TurnStart { .. }))
        .context("Delivery session has no turn")?;
    let SessionEvent::TurnStart { id: turn_id, .. } = &session.events[start] else {
        unreachable!()
    };
    ensure!(
        !require_end
            || session.events[start..]
                .iter()
                .any(|event| matches!(event, SessionEvent::TurnEnd{id,..} if id == turn_id)),
        "An unfinished turn cannot be published"
    );
    let mut pending = HashMap::new();
    let mut written = HashMap::new();
    let mut shell_result = None;
    let mut references = vec![];
    let mut answer: Option<(String, String)> = None;
    for event in &session.events[start..] {
        match event {
            SessionEvent::AssistantMessage { id, text, .. } if !text.trim().is_empty() => {
                answer = Some((id.clone(), text.clone()))
            }
            SessionEvent::ToolCall {
                call_id,
                name,
                arguments,
                ..
            } => {
                answer = None; // A pre-tool statement is not the final response.
                pending.insert(call_id.as_str(), (name.as_str(), arguments));
            }
            SessionEvent::ToolResult {
                id,
                call_id,
                ok: true,
                ..
            } => {
                if let Some((name, arguments)) = pending.remove(call_id.as_str()) {
                    if name == "shell" {
                        shell_result = Some(id.clone());
                    }
                    for path in called_paths(name, arguments) {
                        let path = if Path::new(&path).is_absolute() {
                            PathBuf::from(path)
                        } else {
                            workspace.join(path)
                        };
                        if let Ok(path) = path.canonicalize() {
                            written.insert(path, id.clone());
                        }
                    }
                    if name == "web_crawl" {
                        if let Some(url) = arguments.get("url").and_then(Value::as_str) {
                            references.push(json!({"url":url,"event_id":id,"tool":"web_crawl"}));
                        }
                    }
                }
            }
            SessionEvent::ToolResult { call_id, .. } => {
                pending.remove(call_id.as_str());
            }
            _ => {}
        }
    }
    let mut artifacts = vec![];
    let mut seen = HashSet::new();
    for declared in &agent.files {
        let path = match artifact_path(workspace, declared) {
            Ok(path) => path,
            Err(_) => continue,
        };
        if !seen.insert(path.clone()) {
            continue;
        }
        let bytes = fs::read(&path)?;
        if is_final_report(agent, declared)
            && std::str::from_utf8(&bytes).is_ok_and(is_internal_report_packet)
        {
            ensure!(!require_complete, "Assigned delivery is incomplete: {declared} contains an internal ARIS packet. Write the report for the reader using its facts and sources.");
            continue;
        }
        let digest = hash(&bytes);
        let mut event_id = written.get(&path).cloned();
        if event_id.is_none() {
            if let Some(proof) = proof {
                if proof.exclusive_files.contains(declared)
                    && proof
                        .before_files
                        .get(declared)
                        .is_some_and(|before| before.as_ref() != Some(&digest))
                {
                    event_id = shell_result.clone();
                }
                if event_id.is_none()
                    && proof
                        .before_files
                        .get(declared)
                        .is_some_and(|before| before.as_ref() == Some(&digest))
                {
                    if let Some(previous) = proof
                        .prior_artifacts
                        .iter()
                        .find(|artifact| artifact.path == *declared && artifact.sha256 == digest)
                    {
                        let mut previous = previous.clone();
                        previous.reused = true;
                        artifacts.push(previous);
                        continue;
                    }
                }
            }
        }
        let Some(event_id) = event_id else { continue };
        let size_bytes = bytes.len();
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };
        if text.contains('\0') {
            continue;
        }
        artifacts.push(DeliveryArtifact {
            path: declared.clone(),
            text,
            sha256: digest,
            size_bytes,
            event_id,
            task_id: task_id.clone(),
            run_id: run_id.clone(),
            reused: false,
        });
    }
    if require_complete && agent.delivery.is_some() {
        for declared in agent.files.iter().filter(|path| is_text_output(path)) {
            ensure!(artifacts.iter().any(|artifact|artifact.path==*declared),"Assigned delivery is incomplete: {declared} has no verified UTF-8 output for this turn");
        }
    }
    let (event_id, text) = answer.unwrap_or_default();
    ensure!(
        !require_complete || !text.trim().is_empty() || !artifacts.is_empty(),
        "Completed role has no final response or verified UTF-8 artifact"
    );
    Ok(DeliverySource {
        agent_id: agent.id.clone(),
        session_id: agent.session_id.clone(),
        task_id,
        run_id,
        turn_id: turn_id.clone(),
        event_id,
        text,
        artifacts,
        references,
    })
}

pub(crate) fn ancestors<'a>(team: &'a Team, agent: &TeamAgent) -> Result<Vec<&'a TeamAgent>> {
    fn visit<'a>(
        team: &'a Team,
        id: &str,
        seen: &mut HashSet<String>,
        out: &mut Vec<&'a TeamAgent>,
    ) -> Result<()> {
        if !seen.insert(id.into()) {
            return Ok(());
        }
        let agent = team
            .agents
            .iter()
            .find(|agent| agent.id == id)
            .context("Delivery dependency missing")?;
        for parent in &agent.depends_on {
            visit(team, parent, seen, out)?;
        }
        out.push(agent);
        Ok(())
    }
    let mut result = vec![];
    let mut seen = HashSet::new();
    for id in &agent.depends_on {
        visit(team, id, &mut seen, &mut result)?;
    }
    ensure!(!seen.contains(&agent.id), "Delivery dependency cycle");
    Ok(result)
}

fn relevant_tunnels(team: &Team, ids: &HashSet<String>) -> Result<Vec<Value>> {
    Ok(team
        .tunnels
        .iter()
        .filter(|tunnel| tunnel.participants.iter().any(|id| ids.contains(id)))
        .map(serde_json::to_value)
        .collect::<serde_json::Result<_>>()?)
}

fn tunnel_semantics(tunnels: &[Value]) -> Vec<Value> {
    let mut evidence = Vec::new();
    for tunnel in tunnels {
        // A courtesy notification stays in the transcript without invalidating a published report.
        let messages: Vec<_> = tunnel.get("messages").and_then(Value::as_array).into_iter().flatten()
            .filter(|message| message["kind"] != "notice")
            .map(|message| {
                let mut evidence = json!({
                "id":message["id"], "from":message["from"], "to":message["to"], "text":message["text"],
                "kind":message["kind"], "proposal_id":message["proposal_id"], "reply_to":message["reply_to"]
                });
                if let Some(data) = message.get("data").filter(|value| !value.is_null()) {
                    evidence["data"] = data.clone();
                }
                evidence
            })
            .collect();
        let decision = tunnel.get("decision").filter(|value| !value.is_null())
            .map(|value| json!({"proposal_id":value["proposal_id"],"text":value["text"],"accepted_by":value["accepted_by"]}));
        if !messages.is_empty() || decision.is_some() {
            evidence.push(json!({"id":tunnel["id"],"participants":tunnel["participants"],"topic":tunnel["topic"],
                "messages":messages,"decision":decision}));
        }
    }
    evidence.sort_by_key(|tunnel| tunnel["id"].as_str().unwrap_or("").to_owned());
    evidence
}

/// Merge only material actually available before the final model response.
/// Scheduler delivery timestamps and automatic closure are not new evidence.
fn observed_tunnels(
    prepared: &[Value],
    current: &[Value],
    session: &Session,
    agent_id: &str,
    turn_id: &str,
) -> Vec<Value> {
    let mut known_tunnels = HashSet::new();
    let mut seen = HashSet::new();
    let remember =
        |tunnel: &Value, known: &mut HashSet<String>, seen: &mut HashSet<(String, String)>| {
            if let Some(id) = tunnel.get("id").and_then(Value::as_str) {
                known.insert(id.to_owned());
                for message in tunnel
                    .get("messages")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if let Some(message_id) = message.get("id").and_then(Value::as_str) {
                        seen.insert((id.to_owned(), message_id.to_owned()));
                    }
                }
            }
        };
    for tunnel in prepared {
        remember(tunnel, &mut known_tunnels, &mut seen);
    }
    let start = session
        .events
        .iter()
        .rposition(|event| matches!(event,SessionEvent::TurnStart{id,..} if id==turn_id))
        .unwrap_or(session.events.len());
    let last_answer = session.events[start..]
        .iter()
        .rposition(|event| matches!(event, SessionEvent::AssistantMessage { .. }))
        .map(|index| start + index)
        .unwrap_or(start);
    for event in &session.events[start..last_answer] {
        match event {
            SessionEvent::SystemNote { id, .. } => {
                if let Some(parts) = id
                    .strip_prefix("tunnel:")
                    .and_then(|value| value.split_once(':'))
                {
                    known_tunnels.insert(parts.0.to_owned());
                    seen.insert((parts.0.to_owned(), parts.1.to_owned()));
                }
            }
            SessionEvent::ToolResult {
                name,
                ok: true,
                content,
                ..
            } if name == "agent_tunnel" => {
                if let Ok(value) = serde_json::from_str::<Value>(content) {
                    for tunnel in value
                        .get("tunnels")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .chain(value.get("tunnel"))
                    {
                        remember(tunnel, &mut known_tunnels, &mut seen);
                    }
                }
            }
            _ => {}
        }
    }
    let mut captured = vec![];
    for tunnel in current {
        let Some(id) = tunnel.get("id").and_then(Value::as_str) else {
            continue;
        };
        let mut copy = tunnel.clone();
        let original = tunnel
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let messages: Vec<_> = original
            .iter()
            .filter(|message| {
                seen.contains(&(
                    id.to_owned(),
                    message
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                )) || (message.get("from").and_then(Value::as_str) == Some(agent_id)
                    && message.get("turn_id").and_then(Value::as_str) == Some(turn_id))
            })
            .cloned()
            .collect();
        if !known_tunnels.contains(id) && messages.is_empty() {
            continue;
        }
        if let Some(proposal) = copy
            .get("decision")
            .and_then(|decision| decision.get("proposal_id"))
            .and_then(Value::as_str)
        {
            if !messages
                .iter()
                .any(|message| message.get("id").and_then(Value::as_str) == Some(proposal))
                || !messages.iter().any(|message| {
                    message.get("kind").and_then(Value::as_str) == Some("accept")
                        && message.get("proposal_id").and_then(Value::as_str) == Some(proposal)
                })
            {
                copy.as_object_mut().unwrap().remove("decision");
            }
        }
        copy["messages"] = json!(messages);
        captured.push(copy);
    }
    captured
}

fn prepare_files(runtime: &Runtime, team: &Team, agent: &TeamAgent) -> Result<PreparedDelivery> {
    let prior_artifacts = load(runtime, team, agent)?
        .map(|snapshot| snapshot.owner.artifacts)
        .unwrap_or_default();
    let before_files: HashMap<_, _> = agent
        .files
        .iter()
        .map(|path| {
            (
                path.clone(),
                artifact_path(&runtime.workspace_root, path)
                    .ok()
                    .and_then(|resolved| file_digest(&resolved).ok()),
            )
        })
        .collect();
    let exclusive_files: HashSet<_> = agent
        .files
        .iter()
        .filter(|path| {
            !team.agents.iter().any(|other| {
                other.id != agent.id
                    && other.status == "running"
                    && other
                        .files
                        .iter()
                        .any(|claim| claim.eq_ignore_ascii_case(path))
            })
        })
        .cloned()
        .collect();
    Ok(PreparedDelivery {
        prompt: String::new(),
        sources: vec![],
        tunnels: vec![],
        before_files,
        exclusive_files,
        prior_artifacts,
    })
}

fn public_evidence_url(raw: &str) -> Option<String> {
    let url = reqwest::Url::parse(raw).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || raw.len() > 8192
    {
        return None;
    }
    let host = url
        .host_str()?
        .trim_matches(['[', ']'])
        .to_ascii_lowercase();
    if host == "localhost"
        || host.ends_with(".localhost")
        || host.ends_with(".local")
        || host.ends_with(".internal")
    {
        return None;
    }
    if let Ok(address) = host.parse::<std::net::IpAddr>() {
        let private = match address {
            std::net::IpAddr::V4(ip) => {
                ip.is_private()
                    || ip.is_loopback()
                    || ip.is_link_local()
                    || ip.is_unspecified()
                    || ip.is_multicast()
                    || ip.is_broadcast()
            }
            std::net::IpAddr::V6(ip) => {
                ip.is_loopback()
                    || ip.is_unspecified()
                    || ip.is_multicast()
                    || (ip.segments()[0] & 0xfe00) == 0xfc00
                    || (ip.segments()[0] & 0xffc0) == 0xfe80
                    || ip
                        .to_ipv4_mapped()
                        .is_some_and(|ip| ip.is_private() || ip.is_loopback() || ip.is_link_local())
            }
        };
        if private {
            return None;
        }
    } else if !host.contains('.') {
        return None;
    }
    Some(url.to_string())
}

fn write_web_evidence(runtime: &Runtime, team: &Team, content: &str) -> Result<String> {
    uuid::Uuid::parse_str(&team.id).context("Invalid evidence team id")?;
    write_evidence(runtime, &team.id, content)
}

fn write_evidence(runtime: &Runtime, scope: &str, content: &str) -> Result<String> {
    let scope = uuid::Uuid::parse_str(scope)
        .map(|id| id.to_string())
        .unwrap_or_else(|_| hash(scope.as_bytes()));
    let workspace = runtime.workspace_root.canonicalize()?;
    let mut directory = workspace.clone();
    for component in [".aris", "evidence", scope.as_str()] {
        directory.push(component);
        if !directory.try_exists()? {
            match fs::create_dir(&directory) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        let metadata = fs::symlink_metadata(&directory)?;
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "Evidence directory must not be a symlink"
        );
        ensure!(
            directory.canonicalize()?.starts_with(&workspace),
            "Evidence directory escaped the workspace"
        );
    }
    let digest = hash(content.as_bytes());
    let filename = format!("{digest}.txt");
    let path = directory.join(&filename);
    // All publishers keep the same lock file. Once the first complete record
    // exists, later writers verify it without replacing an active reader's file.
    let lock_path = directory.join(".publish.lock");
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true);
    let lock = match options.clone().create_new(true).open(&lock_path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            ensure!(
                fs::symlink_metadata(&lock_path)?.file_type().is_file()
                    && lock_path.canonicalize()?.starts_with(&directory),
                "Evidence publication lock must be a regular file in the evidence directory"
            );
            options.open(&lock_path)?
        }
        Err(error) => return Err(error.into()),
    };
    fs2::FileExt::lock_exclusive(&lock).context("Lock evidence publication")?;
    if path.try_exists()? {
        let metadata = fs::symlink_metadata(&path)?;
        ensure!(
            metadata.is_file()
                && !metadata.file_type().is_symlink()
                && path.canonicalize()?.starts_with(&workspace),
            "Evidence file must remain inside the workspace"
        );
        ensure!(
            metadata.len() == content.len() as u64 && file_digest(&path)? == digest,
            "Evidence cache content hash mismatch"
        );
    } else {
        // Publish a complete record atomically, including if two roles cached
        // identical responses concurrently. Never expose a partial cache file.
        let temporary = directory.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| -> Result<()> {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(content.as_bytes())?;
            drop(file);
            fs::rename(&temporary, &path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result?;
    }
    Ok(format!(".aris/evidence/{scope}/{filename}"))
}

fn web_record_quality(tool: &str, content: &str) -> (bool, bool, usize) {
    let parsed = serde_json::from_str::<Value>(content).ok();
    let pages = parsed
        .as_ref()
        .and_then(|value| value.get("pages"))
        .and_then(Value::as_array);
    let complete = !content.contains("…[truncated]")
        && !parsed
            .as_ref()
            .and_then(|value| value.get("truncated"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
    let successful_pages = pages
        .into_iter()
        .flatten()
        .filter(|page| {
            page.get("status")
                .and_then(Value::as_u64)
                .is_some_and(|status| (200..300).contains(&status))
                && page
                    .get("markdown")
                    .and_then(Value::as_str)
                    .is_some_and(|text| !text.trim().is_empty())
        })
        .count();
    let successful = if tool == "web_fetch" {
        content
            .split_whitespace()
            .nth(1)
            .and_then(|status| status.parse::<u16>().ok())
            .is_some_and(|status| (200..300).contains(&status))
    } else {
        successful_pages > 0
    };
    (successful, complete, successful_pages)
}

/// Only successful, recorded web-tool results are copied. This never fetches a
/// URL or treats an incomplete history excerpt as a fresh/full web observation.
fn recorded_web_evidence(runtime: &Runtime, team: &Team, agent: &TeamAgent) -> Result<Vec<Value>> {
    let mut owners = vec![agent];
    owners.extend(ancestors(team, agent)?);
    let mut records = vec![];
    let mut seen = HashSet::new();
    for owner in owners {
        let Ok(session) = runtime.sessions.get_or_load(&owner.session_id) else {
            continue;
        };
        let session = session.read();
        let mut pending = HashMap::new();
        let mut candidates = vec![];
        for event in &session.events {
            match event {
                SessionEvent::TurnStart { .. } => pending.clear(),
                SessionEvent::ToolCall {
                    call_id,
                    name,
                    arguments,
                    ..
                } if matches!(name.as_str(), "web_crawl" | "web_fetch") => {
                    if let Some(url) = arguments
                        .get("url")
                        .and_then(Value::as_str)
                        .and_then(public_evidence_url)
                    {
                        pending.insert(call_id.as_str(), (name.as_str(), url));
                    }
                }
                SessionEvent::ToolResult {
                    id,
                    call_id,
                    name,
                    ok,
                    content,
                    at,
                } => {
                    let Some((called_name, url)) = pending.remove(call_id.as_str()) else {
                        continue;
                    };
                    if !ok || name != called_name || content.is_empty() || content.contains('\0') {
                        continue;
                    }
                    candidates.push((id, called_name, url, content, at));
                }
                _ => {}
            }
        }
        // Prefer complete successful observations, then recency, before applying
        // the distinct-URL cap. Repeated refreshes cannot crowd out other sources.
        candidates
            .sort_by_key(|(_, tool, _, content, at)| (web_record_quality(tool, content), **at));
        for (id, tool, url, content, at) in candidates.into_iter().rev() {
            let digest = hash(content.as_bytes());
            if !seen.insert((owner.id.clone(), url.clone())) {
                continue;
            }
            let parsed = serde_json::from_str::<Value>(content).ok();
            let pages: Vec<_> = parsed.as_ref().and_then(|value| value.get("pages")).and_then(Value::as_array).into_iter().flatten().filter_map(|page| {
                let source_url = page.get("source_url").or_else(|| page.get("url")).and_then(Value::as_str).and_then(public_evidence_url)?;
                Some(json!({"url":source_url,"fetched_at":page.get("fetched_at"),"http_status":page.get("status"),"truncated":page.get("truncated")}))
            }).collect();
            let path = write_web_evidence(runtime, team, content)?;
            let mut record = json!({
                "owner_agent_id":owner.id,"session_id":owner.session_id,"event_id":id,"tool":tool,"url":url,
                "observed_at":at,"pages":pages,"content_path":path,"sha256":digest,"size_bytes":content.len(),
                "recorded_content_truncated":content.contains("…[truncated]") || parsed.as_ref().and_then(|value|value.get("truncated")).and_then(Value::as_bool).unwrap_or(false),
                "preview":content.chars().take(240).collect::<String>(),
            });
            if content.chars().nth(PAGE_CHARS).is_some() {
                record["paged_content"] = index_context_text(runtime, &team.id, content)?;
            }
            records.push(record);
        }
    }
    Ok(records)
}

/// A content-addressed, reversible text representation for a model with a
/// finite turn window. Each JSONL line is one UTF-8 page. The reference has
/// constant size even for a many-page input and uses the normal read_file tool.
pub(crate) fn index_context_text(runtime: &Runtime, scope: &str, text: &str) -> Result<Value> {
    let mut input = text.chars().peekable();
    let mut written = Vec::new();
    let mut count = 0usize;
    while input.peek().is_some() {
        let page: String = input.by_ref().take(PAGE_CHARS).collect();
        count += 1;
        serde_json::to_writer(
            &mut written,
            &json!({"part":count,"size_bytes":page.len(),"sha256":hash(page.as_bytes()),"text":page}),
        )?;
        written.push(b'\n');
    }
    let encoded = std::str::from_utf8(&written)?;
    let path = write_evidence(runtime, scope, encoded)?;
    Ok(
        json!({"content_path":path,"encoding":"jsonl-utf8-parts","part_count":count,"size_bytes":text.len(),"sha256":hash(text.as_bytes()),
        "read_with":"read_file(path=content_path,offset=1,limit=1); read successive lines and join their text fields in order."}),
    )
}

/// Preserve provenance for a structured record while exposing just its identity
/// and the immutable body reference in the prompt.
fn index_record(runtime: &Runtime, team: &Team, record: &Value) -> Result<Value> {
    let mut index = index_context_text(runtime, &team.id, &serde_json::to_string(record)?)?;
    for key in ["agent_id", "id", "topic", "participants", "url", "path"] {
        if let Some(value) = record.get(key) {
            if serde_json::to_string(value)?
                .chars()
                .nth(PAGE_CHARS)
                .is_none()
            {
                index[key] = value.clone();
            }
        }
    }
    Ok(index)
}

pub fn prepare(
    runtime: &Runtime,
    team: &Team,
    agent: &TeamAgent,
    fallback_prompt: String,
) -> Result<PreparedDelivery> {
    if agent.delivery.is_none() {
        return Ok(PreparedDelivery {
            prompt: fallback_prompt,
            sources: vec![],
            tunnels: vec![],
            before_files: HashMap::new(),
            exclusive_files: HashSet::new(),
            prior_artifacts: vec![],
        });
    }
    let mut sources = vec![];
    let PreparedDelivery {
        before_files,
        exclusive_files,
        prior_artifacts,
        ..
    } = prepare_files(runtime, team, agent)?;
    for dependency in ancestors(team, agent)? {
        ensure!(
            dependency.status == "completed",
            "Delivery source {} has not completed",
            dependency.name
        );
        let source = match load(runtime, team, dependency)? {
            Some(snapshot) => {
                ensure!(
                    snapshot.owner.task_id == dependency.task_id
                        && snapshot.owner.run_id == dependency.run_id,
                    "Delivery source {} has no snapshot for its latest run",
                    dependency.name
                );
                snapshot.owner
            }
            None => {
                // Old histories have no immutable file snapshots. Retain their
                // actual completed reply, never label today's file as old output.
                let session = runtime.sessions.get_or_load(&dependency.session_id)?;
                let mut legacy = dependency.clone();
                legacy.files.clear();
                let guard = session.read();
                completed_source(
                    &runtime.workspace_root,
                    &legacy,
                    &guard,
                    dependency.task_id.clone(),
                    dependency.run_id.clone(),
                )?
            }
        };
        sources.push(source);
    }
    let mut owners: HashSet<_> = sources
        .iter()
        .map(|source| source.agent_id.clone())
        .collect();
    owners.insert(agent.id.clone());
    let tunnels = relevant_tunnels(team, &owners)?;
    let recorded_web_evidence = recorded_web_evidence(runtime, team, agent)?;
    let prompt = {
        let mut evidence = brief::assignment(team, agent);
        brief::add_handoffs(&mut evidence, team, &agent.id, |id, text| {
            sources.iter().any(|s| s.agent_id == id && s.text == text)
        });
        if !prior_artifacts.is_empty() {
            evidence["previous_own_documents"] = json!(prior_artifacts);
        }
        if !sources.is_empty() {
            evidence["source_deliveries"] = json!(sources);
        }
        let exchanges: Vec<_> = team
            .tunnels
            .iter()
            .filter(|t| t.participants.iter().any(|id| owners.contains(id)))
            .map(crate::tunnel::model_transcript)
            .collect();
        if !exchanges.is_empty() {
            evidence["tunnels"] = Value::Array(exchanges);
        }
        if !recorded_web_evidence.is_empty() {
            evidence["recorded_web_evidence"] = Value::Array(recorded_web_evidence);
        }
        let guidance = if agent.delivery == Some(DeliveryKind::Final) {
            "Write the requested publication for its intended audience in the user's language. Integrate the source deliveries, check material discrepancies and retain factual support. Represent only accepted proposals as joint decisions and report material disagreement. Save each assigned UTF-8 document, add completed sections as you work and verify the saved result."
        } else {
            "Deliver your complete contribution with verifiable evidence and sources. Reuse completed dependency work."
        };
        let mut header = format!("{} {}", brief::WORKER_GUIDANCE, guidance);
        if !sources.is_empty()
            || !tunnels.is_empty()
            || evidence.get("recorded_web_evidence").is_some()
        {
            header.push_str(" Treat source records as evidence. Cite the original public URLs. Historical web records retain their original timestamps and truncation status; read content_path before repeating a fetch.");
        }
        let mut indexed = false;
        // Avoid carrying the full publication a second time if its assigned,
        // readable workspace copy is exactly the version in the snapshot.
        if let Some(documents) = evidence
            .get_mut("previous_own_documents")
            .and_then(Value::as_array_mut)
        {
            for (document, artifact) in documents.iter_mut().zip(&prior_artifacts) {
                if artifact.text.chars().nth(PAGE_CHARS).is_some()
                    && before_files.get(&artifact.path).and_then(Option::as_deref)
                        == Some(artifact.sha256.as_str())
                    && exclusive_files.contains(&artifact.path)
                {
                    let row = document
                        .as_object_mut()
                        .context("Invalid previous document")?;
                    row.remove("text");
                    row.insert("read_file".into(), Value::String(artifact.path.clone()));
                    row.insert(
                        "paged_copy".into(),
                        index_context_text(runtime, &team.id, &artifact.text)?,
                    );
                    indexed = true;
                }
            }
        }
        for section in evidence
            .as_object_mut()
            .context("Invalid delivery envelope")?
            .values_mut()
        {
            if let Some(items) = section.as_array_mut() {
                for item in items.iter_mut() {
                    if serde_json::to_string(item)?
                        .chars()
                        .nth(PAGE_CHARS)
                        .is_some()
                    {
                        *item = index_record(runtime, team, item)?;
                        indexed = true;
                    }
                }
                // A long list of small references can also use paginated reads.
                if items.len() > 400 {
                    *section = index_record(runtime, team, section)?;
                    indexed = true;
                }
            } else if serde_json::to_string(section)?
                .chars()
                .nth(PAGE_CHARS)
                .is_some()
            {
                *section = index_record(runtime, team, section)?;
                indexed = true;
            }
        }
        let body = serde_json::to_string(&evidence)?;
        if indexed {
            format!("{header}\nRead indexed previous documents using read_file before revising them. A paged_copy preserves the original bytes if a working file changes. For an index with encoding jsonl-utf8-parts, read content_path using offset and limit, then join the text fields in part order to recover the full original. Each page and the complete text has a SHA-256 and byte length. These are internal evidence paths; cite the original official URLs in the publication.\n{body}")
        } else {
            format!("{header}\n{body}")
        }
    };
    Ok(PreparedDelivery {
        prompt,
        sources,
        tunnels,
        before_files,
        exclusive_files,
        prior_artifacts,
    })
}

pub fn capture(
    runtime: &Runtime,
    team: &Team,
    agent: &TeamAgent,
    task_id: &str,
    run_id: Option<&str>,
    prepared: &PreparedDelivery,
) -> Result<()> {
    let task = runtime
        .tasks
        .task(task_id)
        .context("Delivery task missing")?;
    ensure!(
        task.state == TaskState::Completed,
        "Only a completed native task can publish a delivery"
    );
    ensure!(
        task.session_id.as_deref() == Some(agent.session_id.as_str()),
        "Delivery task does not own this session"
    );
    ensure!(
        runtime
            .tasks
            .latest_run_for_task(task_id)
            .map(|run| run.id)
            .as_deref()
            == run_id,
        "Delivery run identity does not match the completed task"
    );
    let session = runtime.sessions.get_or_load(&agent.session_id)?;
    let owner = match completed_source_with_proof(
        &runtime.workspace_root,
        agent,
        &session.read(),
        Some(task_id.into()),
        run_id.map(str::to_owned),
        Some(prepared),
    ) {
        Ok(owner) => owner,
        Err(_) if agent.delivery.is_none() => return Ok(()),
        Err(error) => return Err(error),
    };
    let current = runtime.teams.get(&team.id)?;
    let mut owners: HashSet<_> = prepared
        .sources
        .iter()
        .map(|source| source.agent_id.clone())
        .collect();
    owners.insert(agent.id.clone());
    let tunnels = observed_tunnels(
        &prepared.tunnels,
        &relevant_tunnels(&current, &owners)?,
        &session.read(),
        &agent.id,
        &owner.turn_id,
    );
    save(
        runtime,
        team,
        &Snapshot {
            team_id: team.id.clone(),
            owner,
            sources: prepared.sources.clone(),
            tunnels,
            at: chrono::Utc::now().to_rfc3339(),
        },
    )
}

fn source_changed(runtime: &Runtime, team: &Team, source: &DeliverySource) -> bool {
    let Some(agent) = team.agents.iter().find(|agent| agent.id == source.agent_id) else {
        return true;
    };
    if agent.status != "completed"
        || agent.session_id != source.session_id
        || agent.task_id != source.task_id
        || agent.run_id != source.run_id
    {
        return true;
    }
    source.artifacts.iter().any(|artifact| {
        artifact_path(&runtime.workspace_root, &artifact.path)
            .ok()
            .and_then(|path| file_digest(&path).ok())
            .is_none_or(|digest| digest != artifact.sha256)
    })
}

fn snapshot_stale(runtime: &Runtime, team: &Team, snapshot: &Snapshot) -> Result<bool> {
    let mut owners: HashSet<_> = snapshot
        .sources
        .iter()
        .map(|source| source.agent_id.clone())
        .collect();
    owners.insert(snapshot.owner.agent_id.clone());
    Ok(source_changed(runtime, team, &snapshot.owner)
        || snapshot
            .sources
            .iter()
            .any(|source| source_changed(runtime, team, source))
        || tunnel_semantics(&relevant_tunnels(team, &owners)?)
            != tunnel_semantics(&snapshot.tunnels))
}

/// Read a verified completed reply and its immutable artifacts for any lane.
pub fn current_worker_delivery(
    runtime: &Runtime,
    team: &Team,
    agent: &TeamAgent,
) -> Result<Option<DeliverySource>> {
    let Some(snapshot) = load(runtime, team, agent)? else {
        return Ok(None);
    };
    if snapshot_stale(runtime, team, &snapshot)? {
        return Ok(None);
    }
    Ok(Some(snapshot.owner))
}

pub fn deliverable(runtime: &Runtime, team: &Team) -> Result<Option<Delivery>> {
    let finals: Vec<_> = team
        .agents
        .iter()
        .filter(|agent| agent.delivery == Some(DeliveryKind::Final))
        .collect();
    ensure!(finals.len() <= 1, "Team has multiple final delivery owners");
    let Some(agent) = finals.first() else {
        return Ok(None);
    };
    let Some(snapshot) = load(runtime, team, agent)? else {
        return Ok(Some(Delivery {
            agent_id: agent.id.clone(),
            session_id: agent.session_id.clone(),
            task_id: agent.task_id.clone(),
            run_id: agent.run_id.clone(),
            status: agent.status.clone(),
            text: String::new(),
            artifacts: vec![],
            sources: vec![],
            tunnel_ids: vec![],
            stale: false,
            snapshot_at: None,
        }));
    };
    let stale = snapshot_stale(runtime, team, &snapshot)?;
    let tunnel_ids = snapshot
        .tunnels
        .iter()
        .filter_map(|tunnel| tunnel.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect();
    Ok(Some(Delivery {
        agent_id: agent.id.clone(),
        session_id: agent.session_id.clone(),
        task_id: snapshot.owner.task_id,
        run_id: snapshot.owner.run_id,
        status: agent.status.clone(),
        text: if snapshot.owner.artifacts.is_empty() {
            snapshot.owner.text
        } else {
            String::new()
        },
        artifacts: snapshot.owner.artifacts,
        sources: snapshot.sources,
        tunnel_ids,
        stale,
        snapshot_at: Some(snapshot.at),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AppConfig;
    use aris_llm::{LlmConfig, OpenAiCompatibleClient};
    use aris_tools::ToolRegistry;
    use std::sync::Arc;

    struct TestRoot(PathBuf);
    impl TestRoot {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("aris-delivery-test-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }
        fn runtime(&self) -> Arc<Runtime> {
            let mut config = AppConfig::builtin_default();
            config.agent.scheduler_enabled = false;
            config.paths.outer_home = self.0.join("outer").to_string_lossy().into();
            Runtime::bootstrap(
                config,
                self.0.clone(),
                OpenAiCompatibleClient::new(LlmConfig {
                    base_url: "http://127.0.0.1:1/v1".into(),
                    model: "fixture".into(),
                    api_key: "fixture".into(),
                    ..Default::default()
                })
                .unwrap(),
                Arc::new(ToolRegistry::new()),
            )
            .unwrap()
        }
    }
    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn agent(id: &str, delivery: DeliveryKind, deps: &[&str], files: &[&str]) -> TeamAgent {
        serde_json::from_value(json!({"id":id,"name":id,"role":"arbitrary role","profile_id":"fixture","session_id":format!("session-{id}"),"status":"completed","depends_on":deps,"files":files,"delivery":delivery})).unwrap()
    }
    fn team(agents: Vec<TeamAgent>) -> Team {
        serde_json::from_value(json!({"id":uuid::Uuid::new_v4().to_string(),"name":"publication","goal":"调研后形成可发布稿","status":"completed","agents":agents,"files":[],"handoffs":[],"messages":[],"checkpoints":[],"events":[],"created_at":chrono::Utc::now().to_rfc3339()})).unwrap()
    }
    fn transcript(agent: &TeamAgent, answer: &str) -> Session {
        let mut session = Session::with_id(agent.session_id.clone());
        session.append(SessionEvent::TurnStart {
            id: "turn-1".into(),
            at: chrono::Utc::now(),
            profile_id: Some("fixture".into()),
            model_id: Some("exact-model".into()),
            family: Some("gpt".into()),
        });
        session.append(SessionEvent::AssistantMessage {
            id: "answer-1".into(),
            text: answer.into(),
            reasoning: None,
            provider_items: None,
            at: chrono::Utc::now(),
        });
        session.append(SessionEvent::TurnEnd {
            id: "turn-1".into(),
            at: chrono::Utc::now(),
        });
        session
    }
    fn successful_write(session: &mut Session, path: &str, call: &str) {
        let index = session.events.len() - 2;
        session.events.splice(
            index..index,
            [
                SessionEvent::ToolCall {
                    id: format!("call-event-{call}"),
                    call_id: call.into(),
                    name: "write_file".into(),
                    arguments: json!({"path":path,"content":"actual contents"}),
                    at: chrono::Utc::now(),
                },
                SessionEvent::ToolResult {
                    id: format!("result-{call}"),
                    call_id: call.into(),
                    name: "write_file".into(),
                    ok: true,
                    content: "wrote file".into(),
                    at: chrono::Utc::now(),
                },
            ],
        );
    }
    fn snapshot(runtime: &Runtime, team: &Team, agent: &TeamAgent, answer: &str) -> Snapshot {
        Snapshot {
            team_id: team.id.clone(),
            owner: completed_source(
                &runtime.workspace_root,
                agent,
                &transcript(agent, answer),
                agent.task_id.clone(),
                agent.run_id.clone(),
            )
            .unwrap(),
            sources: vec![],
            tunnels: vec![],
            at: chrono::Utc::now().to_rfc3339(),
        }
    }

    #[test]
    fn captures_actual_multiple_utf8_documents_and_excludes_unowned_files() {
        let root = TestRoot::new();
        let role = agent(
            "writer",
            DeliveryKind::Final,
            &[],
            &["publication.md", "email.txt"],
        );
        fs::write(
            root.0.join("publication.md"),
            "可直接发布的正文，不是已写好。",
        )
        .unwrap();
        fs::write(root.0.join("email.txt"), "第二份邮件正文").unwrap();
        fs::write(root.0.join("unrelated.txt"), "不属于本次交付").unwrap();
        let mut session = transcript(&role, "已写好两份文件。");
        for (path, id) in [
            ("publication.md", "one"),
            ("email.txt", "two"),
            ("unrelated.txt", "other"),
        ] {
            successful_write(&mut session, path, id);
        }
        let source = completed_source(
            &root.0,
            &role,
            &session,
            Some("task".into()),
            Some("run".into()),
        )
        .unwrap();
        assert_eq!(source.artifacts.len(), 2);
        assert_eq!(source.artifacts[0].text, "可直接发布的正文，不是已写好。");
        assert_eq!(
            source.artifacts[0].sha256,
            hash("可直接发布的正文，不是已写好。".as_bytes())
        );
        assert!(source
            .artifacts
            .iter()
            .all(|artifact| artifact.path != "unrelated.txt"));
        assert_eq!(source.text, "已写好两份文件。");
    }

    #[test]
    fn final_reports_require_reader_output_but_machine_deliverables_keep_their_format() {
        let root = TestRoot::new();
        let runtime = root.runtime();
        let report = agent("writer", DeliveryKind::Final, &[], &["report.md"]);
        let plan = prepare_files(&runtime, &team(vec![report.clone()]), &report).unwrap();
        let check = DeliveryCheck {
            workspace: root.0.clone(),
            agent: report.clone(),
            proof: plan,
        };
        let mut session = transcript(&report, "已交付。");
        successful_write(&mut session, "report.md", "w");
        let packet = r#"{"peer_inbox":[{"id":"channel","messages":[{"data":{"cost":12}}]}]}"#;
        for body in [
            packet.to_owned(),
            format!("```json\r\n{packet}\r\n```"),
            r#"{"type":"aris.brief","agent_id":"writer","goal":"summarize"}"#.to_owned(),
            r#"{"action":"ask","tunnel_id":"t","text":"cost?"}"#.to_owned(),
            r#"{"blocking":false,"tunnel":{"id":"t","messages":[]}}"#.to_owned(),
        ] {
            fs::write(root.0.join("report.md"), body).unwrap();
            assert_eq!(check.missing(&session).unwrap(), vec!["report.md"]);
            let error = completed_source(&root.0, &report, &session, None, None).unwrap_err();
            assert!(error.to_string().contains("internal ARIS packet"));
        }
        assert!(check.report_rewrite_hint().unwrap().contains("reader"));
        for body in [
            format!("运营成本为 12 元。\n\n```json\n{packet}\n```"),
            r#"{"count":12}"#.to_owned(),
        ] {
            fs::write(root.0.join("report.md"), &body).unwrap();
            assert!(check.missing(&session).unwrap().is_empty());
            assert_eq!(
                completed_source(&root.0, &report, &session, None, None)
                    .unwrap()
                    .artifacts[0]
                    .text,
                body
            );
        }
        for (kind, name) in [
            (DeliveryKind::Supporting, "report.md"),
            (DeliveryKind::Final, "payload.json"),
            (DeliveryKind::Final, "payload.txt"),
            (DeliveryKind::Final, "data.csv"),
        ] {
            let role = agent("author", kind, &[], &[name]);
            fs::write(root.0.join(name), packet).unwrap();
            let mut own = transcript(&role, "已交付。");
            successful_write(&mut own, name, "w");
            assert_eq!(
                completed_source(&root.0, &role, &own, None, None)
                    .unwrap()
                    .artifacts[0]
                    .text,
                packet
            );
        }
    }

    #[test]
    fn does_not_promote_intermediate_or_failed_tool_output_to_a_final_document() {
        let root = TestRoot::new();
        let role = agent("writer", DeliveryKind::Final, &[], &["old.md"]);
        fs::write(root.0.join("old.md"), "上次留下的旧内容").unwrap();
        let mut session = transcript(&role, "我准备写文件。");
        let index = session.events.len() - 1;
        session.events.splice(
            index..index,
            [
                SessionEvent::ToolCall {
                    id: "call".into(),
                    call_id: "c".into(),
                    name: "write_file".into(),
                    arguments: json!({"path":"old.md"}),
                    at: chrono::Utc::now(),
                },
                SessionEvent::ToolResult {
                    id: "result".into(),
                    call_id: "c".into(),
                    name: "write_file".into(),
                    ok: false,
                    content: "denied".into(),
                    at: chrono::Utc::now(),
                },
            ],
        );
        assert!(completed_source(&root.0, &role, &session, None, None).is_err());
        let mut incomplete = transcript(&role, "尚在生成中");
        incomplete.events.pop();
        assert!(completed_source(&root.0, &role, &incomplete, None, None).is_err());
        assert!(
            completed_source(&root.0, &role, &transcript(&role, "已写好"), None, None).is_err()
        );
        let mut chat_role = role.clone();
        chat_role.files.clear();
        let direct = completed_source(
            &root.0,
            &chat_role,
            &transcript(&role, "这是本轮直接答复"),
            None,
            None,
        )
        .unwrap();
        assert!(direct.artifacts.is_empty()); // existing filenames alone prove nothing
    }

    #[test]
    fn supporting_and_independent_roles_require_their_declared_text_files() {
        let root = TestRoot::new();
        for kind in [DeliveryKind::Supporting, DeliveryKind::Independent] {
            let role = agent("researcher", kind, &[], &["research.md"]);
            let session = transcript(&role, "Would you like me to prepare a report?");
            let error = completed_source(&root.0, &role, &session, None, None)
                .err()
                .unwrap();
            assert!(error.to_string().contains("research.md"));
            // A leftover file without ownership proof cannot satisfy the contract.
            fs::write(root.0.join("research.md"), "unverified previous content").unwrap();
            assert!(completed_source(&root.0, &role, &session, None, None).is_err());
            let mut actual = session.clone();
            successful_write(&mut actual, "research.md", "report");
            let delivered = completed_source(&root.0, &role, &actual, None, None).unwrap();
            assert_eq!(delivered.artifacts.len(), 1);
            let mut legacy = role.clone();
            legacy.delivery = None;
            assert!(completed_source(&root.0, &legacy, &session, None, None).is_ok());
        }
    }

    #[test]
    fn delivery_check_finds_missing_files_and_accepts_same_turn_correction_before_end() {
        let root = TestRoot::new();
        let runtime = root.runtime();
        let role = agent(
            "researcher",
            DeliveryKind::Supporting,
            &[],
            &["research.md"],
        );
        let team = team(vec![role.clone()]);
        let team_dir = runtime.workspace_outer.join("teams").join(&team.id);
        fs::create_dir_all(&team_dir).unwrap();
        fs::write(
            team_dir.join("team.json"),
            serde_json::to_vec(&team).unwrap(),
        )
        .unwrap();
        let mut task =
            aris_protocol::TaskRecord::new(aris_protocol::GoalSpec::new("完成调研报告")).unwrap();
        task.session_id = Some(role.session_id.clone());
        task.owner = Some(aris_protocol::TeamTaskOwner {
            subagent_id: "native-researcher".into(),
            team_id: team.id.clone(),
            agent_id: role.id.clone(),
            parent_session_id: None,
            workspace: root.0.to_string_lossy().into(),
        });
        let task = runtime.tasks.create_task(task).unwrap();
        let check = delivery_check(&runtime, &task.id).unwrap().unwrap();
        assert!(delivery_check(&runtime, "unknown-task").unwrap().is_none());
        let mut session = transcript(&role, "Would you like me to prepare a report?");
        session.events.pop(); // Stop candidate, not a completed turn.
        assert_eq!(check.missing(&session).unwrap(), vec!["research.md"]);
        fs::write(root.0.join("research.md"), "外部文件不算本轮完成").unwrap();
        assert_eq!(check.missing(&session).unwrap(), vec!["research.md"]);
        session.append(SessionEvent::TurnEnd {
            id: "turn-1".into(),
            at: chrono::Utc::now(),
        });
        successful_write(&mut session, "research.md", "correction");
        session.events.pop();
        fs::write(root.0.join("research.md"), "同一轮真实工具补写的完整报告").unwrap();
        assert!(check.missing(&session).unwrap().is_empty());
        // Publication still requires the actual lifecycle to finish.
        assert!(completed_source(&root.0, &role, &session, None, None).is_err());
        session.append(SessionEvent::TurnEnd {
            id: "turn-1".into(),
            at: chrono::Utc::now(),
        });
        let delivered = completed_source(&root.0, &role, &session, None, None).unwrap();
        assert_eq!(delivered.artifacts[0].text, "同一轮真实工具补写的完整报告");
        let another = transcript(&agent("other", DeliveryKind::Supporting, &[], &[]), "other");
        assert!(check.missing(&another).is_err());
    }

    #[test]
    fn recorded_sources_keep_actual_times_and_content_without_refetching_or_repeated_url_loss() {
        let root = TestRoot::new();
        let runtime = root.runtime();
        let researcher = agent("research", DeliveryKind::Supporting, &[], &[]);
        let writer = agent("writer", DeliveryKind::Final, &["research"], &[]);
        let team = team(vec![researcher.clone(), writer.clone()]);
        assert!(recorded_web_evidence(&runtime, &team, &researcher)
            .unwrap()
            .is_empty());
        assert!(!root.0.join(".aris/evidence").exists());
        let mut session = transcript(&researcher, "完成来源核查");
        let at = chrono::DateTime::parse_from_rfc3339("2026-11-19T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let crawl = json!({"pages":[{"source_url":"https://example.com/primary","fetched_at":"2026-11-19T11:59:58Z","status":200,"markdown":"完整第一手原文","truncated":false}],"truncated":false}).to_string();
        let mut add = |id: &str, name: &str, url: &str, content: String, ok: bool| {
            session.append(SessionEvent::ToolCall {
                id: format!("call-{id}"),
                call_id: id.into(),
                name: name.into(),
                arguments: json!({"url":url}),
                at,
            });
            session.append(SessionEvent::ToolResult {
                id: format!("result-{id}"),
                call_id: id.into(),
                name: name.into(),
                ok,
                content,
                at,
            });
        };
        add(
            "primary",
            "web_crawl",
            "https://example.com/primary",
            crawl.clone(),
            true,
        );
        add(
            "other",
            "web_fetch",
            "https://other.example.net/report",
            "HTTP 200 OK\n第二份独立原文".into(),
            true,
        );
        for index in 0..12 {
            add(
                &format!("repeat-{index}"),
                "web_crawl",
                "https://example.com/primary",
                "{\"pages\":[…[truncated]".into(),
                true,
            );
        }
        add(
            "failed",
            "web_fetch",
            "https://example.com/failed",
            "failed".into(),
            false,
        );
        add(
            "private",
            "web_fetch",
            "http://127.0.0.1/secret",
            "HTTP 200 OK\nprivate".into(),
            true,
        );
        add(
            "credential",
            "web_fetch",
            "https://user:password@example.com/secret",
            "HTTP 200 OK\nprivate".into(),
            true,
        );
        add(
            "large",
            "web_fetch",
            "https://example.com/large",
            "x".repeat(132_000),
            true,
        );
        add(
            "partial",
            "web_crawl",
            "https://example.com/partial",
            "{\"pages\":[…[truncated]".into(),
            true,
        );
        runtime.sessions.insert(session);
        let records = recorded_web_evidence(&runtime, &team, &researcher).unwrap();
        assert_eq!(records.len(), 4);
        let large = records
            .iter()
            .find(|row| row["url"] == "https://example.com/large")
            .unwrap();
        assert_eq!(large["size_bytes"], 132_000);
        assert_eq!(large["paged_content"]["sha256"], large["sha256"]);
        let primary = records
            .iter()
            .find(|record| record["url"] == "https://example.com/primary")
            .unwrap();
        assert_eq!(primary["event_id"], "result-primary");
        assert_eq!(primary["observed_at"], "2026-11-19T12:00:00Z");
        assert_eq!(primary["pages"][0]["fetched_at"], "2026-11-19T11:59:58Z");
        let path = root.0.join(primary["content_path"].as_str().unwrap());
        assert_eq!(fs::read_to_string(&path).unwrap(), crawl);
        assert_eq!(primary["sha256"], hash(crawl.as_bytes()));
        assert_eq!(
            records
                .iter()
                .find(|record| record["url"] == "https://example.com/partial")
                .unwrap()["recorded_content_truncated"],
            true
        );
        save(
            &runtime,
            &team,
            &snapshot(&runtime, &team, &researcher, "正式调研正文"),
        )
        .unwrap();
        let prepared = prepare(&runtime, &team, &writer, String::new()).unwrap();
        assert!(prepared.prompt.contains("recorded_web_evidence"));
        assert!(prepared.prompt.contains("https://other.example.net/report"));
        assert!(prepared.prompt.contains("result-primary"));
        fs::write(path, "tampered").unwrap();
        assert!(recorded_web_evidence(&runtime, &team, &researcher)
            .err()
            .unwrap()
            .to_string()
            .contains("hash mismatch"));
    }

    #[test]
    fn evidence_cache_rejects_invalid_urls_and_redirected_parent() {
        for url in [
            "file:///secret",
            "https://user:password@example.com",
            "http://localhost/",
            "http://192.168.1.2/",
            "http://[::1]/",
            "http://[::ffff:127.0.0.1]/",
        ] {
            assert!(public_evidence_url(url).is_none(), "{url}");
        }
        assert!(public_evidence_url("https://example.com/source").is_some());
        let root = TestRoot::new();
        let runtime = root.runtime();
        let team = team(vec![]);
        fs::create_dir_all(root.0.join(".aris")).unwrap();
        fs::write(root.0.join(".aris/evidence"), "not a directory").unwrap();
        assert!(write_web_evidence(&runtime, &team, "original").is_err());
    }

    #[test]
    fn concurrent_evidence_writers_publish_only_complete_identical_content() {
        let root = TestRoot::new();
        let runtime = root.runtime();
        let team = team(vec![]);
        let original = "真实已记录的网页正文\n".repeat(1000);
        let jobs: Vec<_> = (0..8)
            .map(|_| {
                let runtime = runtime.clone();
                let team = team.clone();
                let original = original.clone();
                std::thread::spawn(move || {
                    let path = write_web_evidence(&runtime, &team, &original).unwrap();
                    assert_eq!(
                        fs::read_to_string(runtime.workspace_root.join(&path)).unwrap(),
                        original
                    );
                    path
                })
            })
            .collect();
        let paths: HashSet<_> = jobs.into_iter().map(|job| job.join().unwrap()).collect();
        assert_eq!(paths.len(), 1);
    }

    #[test]
    fn oversized_revision_indexes_only_byte_identical_prior_documents() {
        let root = TestRoot::new();
        let runtime = root.runtime();
        let role = agent(
            "writer",
            DeliveryKind::Final,
            &[],
            &["report.md", "post.md"],
        );
        let team = team(vec![role.clone()]);
        let report = "调查证据".repeat(100_000);
        let post = "Original community post";
        fs::write(root.0.join("report.md"), &report).unwrap();
        fs::write(root.0.join("post.md"), post).unwrap();
        let mut session = transcript(&role, "Both deliverables are ready.");
        successful_write(&mut session, "report.md", "report");
        successful_write(&mut session, "post.md", "post");
        let owner = completed_source(&root.0, &role, &session, None, None).unwrap();
        save(
            &runtime,
            &team,
            &Snapshot {
                team_id: team.id.clone(),
                owner,
                sources: vec![],
                tunnels: vec![],
                at: "first publication".into(),
            },
        )
        .unwrap();
        // A later edit must not replace the captured version in the revision prompt.
        fs::write(root.0.join("post.md"), "external revision").unwrap();
        let prepared = prepare(&runtime, &team, &role, String::new()).unwrap();
        let evidence: Value =
            serde_json::from_str(prepared.prompt.lines().last().unwrap()).unwrap();
        let indexed = &evidence["previous_own_documents"][0];
        assert_eq!(indexed["read_file"], "report.md");
        assert_eq!(indexed["sha256"], hash(report.as_bytes()));
        assert_eq!(indexed["size_bytes"], report.len());
        assert!(indexed.get("text").is_none());
        assert_eq!(evidence["previous_own_documents"][1]["text"], post);
        assert!(prepared.prompt.contains("read_file before revising"));
        assert_eq!(prepared.prior_artifacts[0].text, report);
        assert_eq!(
            load(&runtime, &team, &role)
                .unwrap()
                .unwrap()
                .owner
                .artifacts[0]
                .text,
            report
        );
        fs::write(root.0.join("report.md"), "external revision").unwrap();
        let changed = prepare(&runtime, &team, &role, String::new()).unwrap();
        let summary: Value = serde_json::from_str(changed.prompt.lines().last().unwrap()).unwrap();
        let old = &summary["previous_own_documents"][0];
        assert!(old.get("read_file").is_none());
        let recovered: Value = serde_json::from_str(&reassemble(&root.0, old)).unwrap();
        assert_eq!(recovered["text"], report);
    }

    #[test]
    fn revision_can_read_every_page_of_new_source_evidence_without_truncation() {
        let root = TestRoot::new();
        let runtime = root.runtime();
        let source = agent("source", DeliveryKind::Supporting, &[], &[]);
        let writer = agent("writer", DeliveryKind::Final, &["source"], &["previous.md"]);
        let team = team(vec![source.clone(), writer.clone()]);
        let original = snapshot(&runtime, &team, &source, &"源证据甲🙂\n".repeat(12_000));
        save(&runtime, &team, &original).unwrap();
        let first = prepare(&runtime, &team, &writer, String::new()).unwrap();
        let summary: Value = serde_json::from_str(first.prompt.lines().last().unwrap()).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&reassemble(&root.0, &summary["source_deliveries"][0]))
                .unwrap(),
            serde_json::to_value(&original.owner).unwrap()
        );
        fs::write(root.0.join("previous.md"), "existing publication").unwrap();
        let mut session = transcript(&writer, "Existing published report.");
        successful_write(&mut session, "previous.md", "prior");
        let owner = completed_source(&root.0, &writer, &session, None, None).unwrap();
        save(
            &runtime,
            &team,
            &Snapshot {
                team_id: team.id.clone(),
                owner,
                sources: vec![],
                tunnels: vec![],
                at: "first".into(),
            },
        )
        .unwrap();
        let prepared = prepare(&runtime, &team, &writer, String::new()).unwrap();
        let manifest: Value =
            serde_json::from_str(prepared.prompt.lines().last().unwrap()).unwrap();
        let record = &manifest["source_deliveries"][0];
        assert_eq!(record["agent_id"], "source");
        let recovered = reassemble(&root.0, record);
        assert_eq!(
            serde_json::from_str::<Value>(&recovered).unwrap(),
            serde_json::to_value(original.owner).unwrap()
        );
        assert_eq!(prepared.sources.len(), 1);
    }

    fn reassemble(root: &Path, index: &Value) -> String {
        let pages = fs::read_to_string(root.join(index["content_path"].as_str().unwrap())).unwrap();
        let mut recovered = String::new();
        for (i, line) in pages.lines().enumerate() {
            let part: Value = serde_json::from_str(line).unwrap();
            let body = part["text"].as_str().unwrap();
            assert_eq!(part["part"], i + 1);
            assert_eq!(part["sha256"], hash(body.as_bytes()));
            assert_eq!(part["size_bytes"], body.len());
            assert!(body.chars().count() <= PAGE_CHARS);
            recovered.push_str(body);
        }
        assert_eq!(index["sha256"], hash(recovered.as_bytes()));
        recovered
    }

    #[test]
    fn final_requires_all_text_outputs_and_reuses_only_unchanged_owned_snapshots() {
        let root = TestRoot::new();
        let runtime = root.runtime();
        let role = agent(
            "writer",
            DeliveryKind::Final,
            &[],
            &["publish.md", "mail.txt"],
        );
        let team = team(vec![role.clone()]);
        fs::write(root.0.join("publish.md"), "第一份完成稿").unwrap();
        let mut partial = transcript(&role, "两份都写好了");
        successful_write(&mut partial, "publish.md", "one");
        assert!(completed_source(&root.0, &role, &partial, None, None)
            .err()
            .unwrap()
            .to_string()
            .contains("mail.txt"));
        fs::write(root.0.join("mail.txt"), "第二份完成稿").unwrap();
        successful_write(&mut partial, "mail.txt", "two");
        let owner = completed_source(
            &root.0,
            &role,
            &partial,
            Some("original-task".into()),
            Some("original-run".into()),
        )
        .unwrap();
        save(
            &runtime,
            &team,
            &Snapshot {
                team_id: team.id.clone(),
                owner,
                sources: vec![],
                tunnels: vec![],
                at: "before".into(),
            },
        )
        .unwrap();
        let prepared = prepare(&runtime, &team, &role, String::new()).unwrap();
        assert!(prepared.prompt.contains("第一份完成稿"));
        let retained = completed_source_with_proof(
            &root.0,
            &role,
            &transcript(&role, "协商确认后，原稿无需修改。"),
            Some("next-task".into()),
            Some("next-run".into()),
            Some(&prepared),
        )
        .unwrap();
        assert_eq!(retained.artifacts.len(), 2);
        assert!(retained
            .artifacts
            .iter()
            .all(|artifact| artifact.reused && artifact.run_id.as_deref() == Some("original-run")));
        fs::write(root.0.join("publish.md"), "外部变化，没有本轮成功工具凭证").unwrap();
        assert!(completed_source_with_proof(
            &root.0,
            &role,
            &transcript(&role, "已写好"),
            None,
            None,
            Some(&prepared)
        )
        .is_err());
    }

    #[test]
    fn exclusive_shell_output_uses_before_after_hash_and_successful_call_proof() {
        let root = TestRoot::new();
        let runtime = root.runtime();
        let role = agent("writer", DeliveryKind::Final, &[], &["publish.md"]);
        let team = team(vec![role.clone()]);
        let prepared = prepare(&runtime, &team, &role, String::new()).unwrap();
        fs::write(root.0.join("publish.md"), "由本轮正常shell写出的最终稿").unwrap();
        let mut session = transcript(&role, "已写好");
        session.events.splice(
            1..1,
            [
                SessionEvent::ToolCall {
                    id: "shell-call".into(),
                    call_id: "shell".into(),
                    name: "shell".into(),
                    arguments: json!({"command":"write task-owned publication"}),
                    at: chrono::Utc::now(),
                },
                SessionEvent::ToolResult {
                    id: "shell-proof".into(),
                    call_id: "shell".into(),
                    name: "shell".into(),
                    ok: true,
                    content: "exit 0".into(),
                    at: chrono::Utc::now(),
                },
            ],
        );
        let delivered = completed_source_with_proof(
            &root.0,
            &role,
            &session,
            Some("task".into()),
            Some("run".into()),
            Some(&prepared),
        )
        .unwrap();
        assert_eq!(delivered.artifacts[0].event_id, "shell-proof");
        assert!(!delivered.artifacts[0].reused);
        let mut ambiguous = prepared;
        ambiguous.exclusive_files.clear();
        assert!(completed_source_with_proof(
            &root.0,
            &role,
            &session,
            None,
            None,
            Some(&ambiguous)
        )
        .is_err());
    }

    #[test]
    fn preserves_frozen_publication_and_supporting_report_after_files_change() {
        let root = TestRoot::new();
        let runtime = root.runtime();
        let mut writer = agent(
            "writer",
            DeliveryKind::Final,
            &["research"],
            &["publication.md"],
        );
        writer.task_id = Some("task-old".into());
        writer.run_id = Some("run-old".into());
        let researcher = agent("research", DeliveryKind::Supporting, &[], &["research.md"]);
        let mut team = team(vec![researcher.clone(), writer.clone()]);
        fs::write(root.0.join("publication.md"), "最终推广正文").unwrap();
        fs::write(
            root.0.join("research.md"),
            "完整调研报告及 https://example.test/source",
        )
        .unwrap();
        let mut report_session = transcript(&researcher, "调研报告已保存");
        successful_write(&mut report_session, "research.md", "report");
        let report = completed_source(&root.0, &researcher, &report_session, None, None).unwrap();
        let mut final_session = transcript(&writer, "已写好");
        successful_write(&mut final_session, "publication.md", "final");
        let owner = completed_source(
            &root.0,
            &writer,
            &final_session,
            writer.task_id.clone(),
            writer.run_id.clone(),
        )
        .unwrap();
        save(
            &runtime,
            &team,
            &Snapshot {
                team_id: team.id.clone(),
                owner,
                sources: vec![report],
                tunnels: vec![],
                at: "2026-11-19T00:00:00Z".into(),
            },
        )
        .unwrap();
        let delivered = deliverable(&runtime, &team).unwrap().unwrap();
        assert!(delivered.text.is_empty());
        assert_eq!(delivered.artifacts[0].text, "最终推广正文");
        assert!(delivered.sources[0].artifacts[0]
            .text
            .contains("完整调研报告"));
        assert!(!delivered.stale);
        fs::write(root.0.join("publication.md"), "人工覆盖后的内容").unwrap();
        let changed = deliverable(&runtime, &team).unwrap().unwrap();
        assert!(changed.stale);
        assert_eq!(changed.artifacts[0].text, "最终推广正文");
        team.agents[1].status = "running".into();
        team.agents[1].task_id = Some("new-task".into());
        let running = deliverable(&runtime, &team).unwrap().unwrap();
        assert_eq!(running.status, "running");
        assert_eq!(running.run_id.as_deref(), Some("run-old"));
        assert!(running.stale);
        team.agents[1].delivery = Some(DeliveryKind::Independent);
        assert!(deliverable(&runtime, &team).unwrap().is_none());
    }

    #[test]
    fn multi_step_final_prompt_contains_originals_once_and_full_two_sided_decision() {
        let root = TestRoot::new();
        let runtime = root.runtime();
        let a = agent("a", DeliveryKind::Supporting, &[], &[]);
        let b = agent("b", DeliveryKind::Supporting, &["a"], &[]);
        let writer = agent("writer", DeliveryKind::Final, &["a", "b"], &[]);
        let mut team = team(vec![a.clone(), b.clone(), writer.clone()]);
        for (agent, text) in [(&a, "SOURCE_A_ORIGINAL"), (&b, "SOURCE_B_ORIGINAL")] {
            save(&runtime, &team, &snapshot(&runtime, &team, agent, text)).unwrap();
        }
        let message = |id: &str, from: &str, to: &str, text: &str, kind: &str| json!({"id":id,"seq":1,"from":from,"to":to,"text":text,"kind":kind,"expects_reply":false,"at":"at","session_id":format!("session-{from}"),"task_id":"task","run_id":"run","turn_id":"turn"});
        team.tunnels=serde_json::from_value(json!([{"id":"discussion-1","participants":["a","b"],"topic":"核对主张","status":"closed","created_at":"at","updated_at":"at","messages":[message("m1","a","b","甲的完整论证","proposal"),message("m2","b","a","乙的完整反驳与接受条件","accept")],"decision":{"proposal_id":"m1","text":"双方共同决定的限定结论","accepted_by":["a","b"],"at":"at"}}])).unwrap();
        let figures = json!({"count": 25, "verified": false, "sources": ["https://example.org/results#eligible"]});
        team.tunnels[0].messages[0].data = Some(figures.clone());
        let prepared =
            prepare(&runtime, &team, &writer, "old mixed streaming text".into()).unwrap();
        assert_eq!(prepared.sources.len(), 2);
        assert_eq!(prepared.prompt.matches("SOURCE_A_ORIGINAL").count(), 1);
        assert_eq!(prepared.prompt.matches("SOURCE_B_ORIGINAL").count(), 1);
        let packet: Value = serde_json::from_str(prepared.prompt.lines().last().unwrap()).unwrap();
        assert_eq!(packet["tunnels"][0]["messages"][0]["data"], figures);
        assert!(packet["tunnels"][0]["messages"][0]
            .get("session_id")
            .is_none());
        for exact in [
            "甲的完整论证",
            "乙的完整反驳与接受条件",
            "双方共同决定的限定结论",
        ] {
            assert!(prepared.prompt.contains(exact));
        }
        assert!(!prepared.prompt.contains("old mixed streaming text"));
        assert_eq!(
            prepared.tunnels[0],
            serde_json::to_value(&team.tunnels[0]).unwrap()
        );
        let mut oversized = snapshot(&runtime, &team, &a, &"长".repeat(60_000));
        oversized.owner.event_id = "large".into();
        save(&runtime, &team, &oversized).unwrap();
        let next = prepare(&runtime, &team, &writer, String::new()).unwrap();
        let e: Value = serde_json::from_str(next.prompt.lines().last().unwrap()).unwrap();
        assert!(reassemble(&root.0, &e["source_deliveries"][0]).contains(&"长".repeat(60_000)));
    }

    #[test]
    fn final_observed_coordination_is_frozen_without_scheduler_only_staleness() {
        let role = agent("writer", DeliveryKind::Final, &[], &[]);
        let tunnel = json!({"id":"coordination","participants":["writer","reviewer"],"topic":"事实口径","status":"open","updated_at":"before","messages":[
            {"id":"proposal","from":"writer","to":"reviewer","text":"使用有来源的限定结论","kind":"proposal","turn_id":"turn-1"},
            {"id":"accepted","from":"reviewer","to":"writer","text":"我核对后接受","kind":"accept","proposal_id":"proposal","turn_id":"peer-turn"}
        ],"decision":{"proposal_id":"proposal","text":"使用有来源的限定结论","accepted_by":["writer","reviewer"],"at":"now"}});
        let mut session = transcript(&role, "最终发布稿已包含共同口径。");
        session.events.insert(
            1,
            SessionEvent::ToolResult {
                id: "read-result".into(),
                call_id: "read-call".into(),
                name: "agent_tunnel".into(),
                ok: true,
                content: json!({"tunnels":[tunnel.clone()]}).to_string(),
                at: chrono::Utc::now(),
            },
        );
        let captured = observed_tunnels(&[], &[tunnel.clone()], &session, "writer", "turn-1");
        assert_eq!(
            tunnel_semantics(&captured),
            tunnel_semantics(&[tunnel.clone()])
        );
        let mut closed = tunnel.clone();
        let mut corrected = tunnel.clone();
        corrected["messages"][0]["data"] = json!({"count": 25});
        assert_ne!(tunnel_semantics(&captured), tunnel_semantics(&[corrected]));
        closed["status"] = json!("closed");
        closed["close_reason"] = json!("All workers finished");
        closed["updated_at"] = json!("later");
        closed["messages"][1]["delivered_at"] = json!("now");
        closed["messages"][1]["resumed_at"] = json!("later");
        let receipt =
            json!({"id":"receipt","from":"writer","to":"reviewer","text":"已采纳","kind":"notice"});
        closed["messages"]
            .as_array_mut()
            .unwrap()
            .push(receipt.clone());
        let receipt_only = json!({"id":"courtesy","messages":[receipt]});
        assert!(tunnel_semantics(&[receipt_only]).is_empty());
        assert_eq!(
            tunnel_semantics(&captured),
            tunnel_semantics(&[closed.clone()])
        );
        closed["messages"].as_array_mut().unwrap().push(json!({"id":"late","from":"reviewer","to":"writer","text":"刚发现一项新事实","kind":"message","turn_id":"later-peer-turn"}));
        assert_ne!(
            tunnel_semantics(&captured),
            tunnel_semantics(&[closed.clone()])
        );
        // A note injected after the author's last response is not evidence it used.
        session.append(SessionEvent::SystemNote {
            id: "tunnel:coordination:late".into(),
            text: "刚发现一项新事实".into(),
            at: chrono::Utc::now(),
        });
        let unobserved = observed_tunnels(&[], &[closed.clone()], &session, "writer", "turn-1");
        assert_eq!(unobserved[0]["messages"].as_array().unwrap().len(), 2);
        // A real subsequent model response makes the injected message usable.
        session.append(SessionEvent::AssistantMessage {
            id: "revised".into(),
            text: "已据新事实修订正文。".into(),
            reasoning: None,
            provider_items: None,
            at: chrono::Utc::now(),
        });
        let revised = observed_tunnels(&[], &[closed.clone()], &session, "writer", "turn-1");
        assert_eq!(tunnel_semantics(&revised), tunnel_semantics(&[closed]));
    }
}
