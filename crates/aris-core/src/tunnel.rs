//! Bounded, nonblocking peer coordination between registered native workers.
//! Team.tunnels is the durable source of truth; tools never wait for another
//! agent or start a second competing runner for a live conversation.
use crate::harness::{selected_for_run, Team, TeamAgent, TeamStore};
use crate::{Runtime, SessionEvent};
use anyhow::{ensure, Context, Result};
use aris_tools::{ToolContext, ToolDefinition, ToolError, ToolHandler, ToolMetadata};
use async_trait::async_trait;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::{Arc, Weak};
use uuid::Uuid;

const MAX_TUNNELS: usize = 8;
const MAX_OPEN_PER_AGENT: usize = 2;
const MAX_MESSAGES: usize = 16;
const MAX_MESSAGE_CHARS: usize = 2048;
const MAX_TRANSCRIPT_CHARS: usize = 16_384;
const MAX_FOLLOWUPS_PER_AGENT: u8 = 2;
pub(crate) const MAX_FOLLOWUPS: usize = 8;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelMessage {
    pub id: String,
    pub seq: usize,
    pub from: String,
    pub to: String,
    pub text: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposal_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    pub expects_reply: bool,
    pub at: String,
    pub session_id: String,
    pub task_id: String,
    pub run_id: String,
    pub turn_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumed_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelDecision {
    pub proposal_id: String,
    pub text: String,
    pub accepted_by: Vec<String>,
    pub at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTunnel {
    pub id: String,
    pub participants: [String; 2],
    pub topic: String,
    pub status: String,
    pub messages: Vec<TunnelMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<TunnelDecision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub close_reason: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

fn now() -> String {
    Utc::now().to_rfc3339()
}
fn id() -> String {
    Uuid::new_v4().to_string()
}
fn text<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    optional_text(args, key).with_context(|| format!("{key} is required"))
}

/// Providers may populate every optional tool property with null or an empty
/// string. Both mean omitted; meaningful strings retain their exact contents.
fn optional_text<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
}

struct Actor {
    team_id: String,
    agent_id: String,
    session_id: String,
    task_id: String,
    run_id: String,
    turn_id: String,
}

fn actor(runtime: &Runtime, session_id: &str) -> Result<Actor> {
    let task = runtime
        .tasks
        .task_for_session(session_id)
        .context("No native worker task for this conversation")?;
    let owner = task
        .owner
        .as_ref()
        .context("Only a registered team worker can use agent_tunnel")?;
    let team = runtime.teams.get(&owner.team_id)?;
    let agent = team
        .agents
        .iter()
        .find(|agent| agent.id == owner.agent_id)
        .context("Worker missing")?;
    ensure!(
        team.status == "running"
            && agent.status == "running"
            && agent.session_id == session_id
            && agent.task_id.as_deref() == Some(task.id.as_str())
            && agent.subagent_id.as_deref() == Some(owner.subagent_id.as_str()),
        "Worker is not active in its team"
    );
    let run = runtime
        .tasks
        .latest_run_for_task(&task.id)
        .context("Worker run missing")?;
    let session = runtime.sessions.get_or_load(session_id)?;
    let turn_id = session
        .read()
        .events
        .iter()
        .rev()
        .find_map(|event| match event {
            SessionEvent::TurnStart { id, .. } => Some(id.clone()),
            _ => None,
        })
        .context("Worker turn missing")?;
    Ok(Actor {
        team_id: team.id,
        agent_id: agent.id.clone(),
        session_id: session_id.into(),
        task_id: task.id,
        run_id: run.id,
        turn_id,
    })
}

fn append(
    tunnel: &mut AgentTunnel,
    actor: &Actor,
    kind: &str,
    body: String,
    reply_to: Option<String>,
    proposal_id: Option<String>,
    expects_reply: bool,
) -> Result<()> {
    ensure!(
        !body.trim().is_empty() && body.chars().count() <= MAX_MESSAGE_CHARS,
        "Message needs 1–2048 characters"
    );
    ensure!(
        tunnel.messages.len() < MAX_MESSAGES,
        "Tunnel message limit reached; close it with the unresolved result"
    );
    ensure!(
        tunnel
            .messages
            .iter()
            .map(|message| message.text.chars().count())
            .sum::<usize>()
            + body.chars().count()
            <= MAX_TRANSCRIPT_CHARS,
        "Tunnel transcript limit reached; close it with the unresolved result"
    );
    let peer = tunnel
        .participants
        .iter()
        .find(|agent| **agent != actor.agent_id)
        .unwrap()
        .clone();
    if let Some(reply_to) = &reply_to {
        ensure!(
            tunnel
                .messages
                .iter()
                .any(|message| &message.id == reply_to && message.to == actor.agent_id),
            "reply_to must identify an actual message received from this peer"
        );
    }
    let at = now();
    tunnel.messages.push(TunnelMessage {
        id: id(),
        seq: tunnel.messages.len() + 1,
        from: actor.agent_id.clone(),
        to: peer,
        text: body,
        kind: kind.into(),
        proposal_id,
        reply_to,
        expects_reply,
        at: at.clone(),
        session_id: actor.session_id.clone(),
        task_id: actor.task_id.clone(),
        run_id: actor.run_id.clone(),
        turn_id: actor.turn_id.clone(),
        delivered_at: None,
        resumed_at: None,
    });
    tunnel.updated_at = at;
    Ok(())
}

fn perform(runtime: &Runtime, actor: &Actor, args: &Value) -> Result<Value> {
    let action = text(args, "action")?;
    if action == "read" {
        let team = runtime.teams.get(&actor.team_id)?;
        let requested_id = optional_text(args, "tunnel_id");
        let tunnels: Vec<_> = team
            .tunnels
            .iter()
            .filter(|tunnel| {
                tunnel.participants.contains(&actor.agent_id)
                    && requested_id.is_none_or(|id| tunnel.id == id)
            })
            .collect();
        ensure!(
            !tunnels.is_empty() || requested_id.is_none(),
            "Tunnel not found for this worker"
        );
        return Ok(json!({"tunnels":tunnels,"blocking":false}));
    }
    let mut selected_id = None;
    let team = runtime.teams.mutate(&actor.team_id, |team| {
        ensure!(
            team.status == "running",
            "Team is stopping; coordination is closed"
        );
        let index = if action == "open" {
            let to = text(args, "to")?;
            let topic = text(args, "topic")?;
            ensure!(
                topic.chars().count() <= 160,
                "Topic must be at most 160 characters"
            );
            ensure!(to != actor.agent_id, "A tunnel needs two different workers");
            let peer = team
                .agents
                .iter()
                .find(|agent| agent.id == to)
                .context("Peer worker not found in this team")?;
            ensure!(
                peer.subagent_id.is_some() && selected_for_run(team, peer),
                "Peer must be a worker selected for this dispatch"
            );
            ensure!(
                !matches!(peer.status.as_str(), "failed" | "cancelled"),
                "Peer is unavailable; retry that worker before consulting it"
            );
            if let Some(index) = team.tunnels.iter().position(|tunnel| {
                tunnel.status == "open"
                    && tunnel.participants.contains(&actor.agent_id)
                    && tunnel.participants.iter().any(|agent| agent == to)
                    && tunnel.topic == topic
            }) {
                index
            } else {
                ensure!(
                    team.tunnels.len() < MAX_TUNNELS,
                    "This workflow has reached its 8-tunnel limit"
                );
                for agent in [&actor.agent_id, &to.to_owned()] {
                    ensure!(
                        team.tunnels
                            .iter()
                            .filter(|tunnel| tunnel.status == "open"
                                && tunnel.participants.contains(agent))
                            .count()
                            < MAX_OPEN_PER_AGENT,
                        "A worker can have at most two open tunnels"
                    );
                }
                let at = now();
                team.tunnels.push(AgentTunnel {
                    id: id(),
                    participants: [actor.agent_id.clone(), to.into()],
                    topic: topic.into(),
                    status: "open".into(),
                    messages: vec![],
                    decision: None,
                    close_reason: None,
                    created_at: at.clone(),
                    updated_at: at,
                });
                team.tunnels.len() - 1
            }
        } else {
            let tunnel_id = text(args, "tunnel_id")?;
            team.tunnels
                .iter()
                .position(|tunnel| {
                    tunnel.id == tunnel_id && tunnel.participants.contains(&actor.agent_id)
                })
                .context("Tunnel not found for this worker")?
        };
        let tunnel = &mut team.tunnels[index];
        selected_id = Some(tunnel.id.clone());
        ensure!(
            tunnel.status == "open",
            "Tunnel is closed; its transcript remains readable"
        );
        // Opening addresses a peer and topic, never an existing message. Some
        // providers fill unused references with strings such as "none"; those
        // fields have no meaning for open. Reply actions remain strict.
        let reply_to = if action == "open" {
            None
        } else {
            optional_text(args, "reply_to").map(str::to_owned)
        };
        match action {
            "open" | "send" | "propose" => {
                if action != "open" || optional_text(args, "text").is_some() {
                    let body = text(args, "text")?.to_owned();
                    let expects_reply = action == "propose"
                        || args
                            .get("expects_reply")
                            .and_then(Value::as_bool)
                            .unwrap_or(reply_to.is_none());
                    append(
                        tunnel,
                        actor,
                        if action == "propose" {
                            "proposal"
                        } else {
                            "message"
                        },
                        body,
                        reply_to,
                        None,
                        expects_reply,
                    )?;
                }
            }
            "accept" => {
                let proposal_id = text(args, "proposal_id")?;
                let proposal = tunnel
                    .messages
                    .iter()
                    .find(|message| {
                        message.id == proposal_id
                            && message.kind == "proposal"
                            && message.to == actor.agent_id
                    })
                    .context("Only the other participant's actual proposal may be accepted")?
                    .clone();
                append(
                    tunnel,
                    actor,
                    "accept",
                    optional_text(args, "text")
                        .unwrap_or("I accept this proposal.")
                        .into(),
                    Some(proposal.id.clone()),
                    Some(proposal.id.clone()),
                    false,
                )?;
                tunnel.decision = Some(TunnelDecision {
                    proposal_id: proposal.id,
                    text: proposal.text,
                    accepted_by: vec![proposal.from, actor.agent_id.clone()],
                    at: now(),
                });
                tunnel.status = "closed".into();
                tunnel.close_reason = Some("Both participants accepted the proposal".into());
            }
            "close" => {
                let reason = optional_text(args, "text")
                    .unwrap_or("Closed by a participant without a joint decision");
                ensure!(
                    reason.chars().count() <= MAX_MESSAGE_CHARS,
                    "Close reason is too long"
                );
                tunnel.status = "closed".into();
                tunnel.close_reason = Some(reason.into());
                tunnel.updated_at = now();
            }
            _ => anyhow::bail!("Unknown tunnel action"),
        }
        let event_kind = match action {
            "open" => "tunnel_opened",
            "accept" => "tunnel_decision",
            "close" => "tunnel_closed",
            _ => "tunnel_message",
        };
        let summary = format!("{}: {}", tunnel.id, tunnel.topic);
        TeamStore::event(team, Some(&actor.agent_id), event_kind, &summary);
        Ok(())
    })?;
    let tunnel = team
        .tunnels
        .iter()
        .find(|tunnel| Some(&tunnel.id) == selected_id.as_ref())
        .unwrap();
    Ok(
        json!({"tunnel":tunnel,"blocking":false,"notice":"Delivery is asynchronous. Continue your work; do not poll or wait in a loop. Reply with reply_to, or accept/close explicitly."}),
    )
}

struct TunnelTool {
    runtime: Weak<Runtime>,
}
#[async_trait]
impl ToolHandler for TunnelTool {
    fn definition(&self) -> ToolDefinition {
        let mut metadata = ToolMetadata::read_only();
        // This changes only internal conversation state, never workspace files
        // or an external account. It remains available to read-only workers.
        metadata.idempotent = false;
        ToolDefinition::builtin("agent_tunnel",
            "Consult another real worker in this team through a temporary, asynchronous two-way tunnel. open uses to+topic and optional text; send/propose use tunnel_id+text. Reply with reply_to and expects_reply=false unless you need another answer. accept uses the other worker's proposal_id and records joint agreement; close records unresolved closure. read returns the full two-way transcript. Never wait or poll: peers receive messages at their next model step; late replies may resume the same conversation within a strict budget.",
            json!({"type":"object","properties":{
                "action":{"type":"string","enum":["open","send","read","propose","accept","close"]},
                "to":{"anyOf":[{"type":"string"},{"type":"null"}]},
                "topic":{"anyOf":[{"type":"string"},{"type":"null"}]},
                "tunnel_id":{"anyOf":[{"type":"string"},{"type":"null"}]},
                "text":{"anyOf":[{"type":"string"},{"type":"null"}]},
                "reply_to":{"anyOf":[{"type":"string"},{"type":"null"}]},
                "proposal_id":{"anyOf":[{"type":"string"},{"type":"null"}]},
                "expects_reply":{"anyOf":[{"type":"boolean"},{"type":"null"},{"enum":[""]}]}
            },"required":["action"],"additionalProperties":false})).with_metadata(metadata)
    }
    async fn call(&self, args: Value, ctx: &ToolContext) -> std::result::Result<String, ToolError> {
        let result: Result<String> = (|| {
            ensure!(!*ctx.cancel.borrow(), "cancelled");
            let runtime = self.runtime.upgrade().context("Runtime stopped")?;
            let actor = actor(&runtime, &ctx.agent_id)?;
            Ok(perform(&runtime, &actor, &args)?.to_string())
        })();
        result.map_err(|error| ToolError::Message(error.to_string()))
    }
}

pub(crate) fn register(runtime: &Arc<Runtime>) {
    runtime.tools.register(Arc::new(TunnelTool {
        runtime: Arc::downgrade(runtime),
    }));
}

fn awaiting(tunnel: &AgentTunnel, message: &TunnelMessage) -> bool {
    tunnel.status == "open"
        && message.expects_reply
        && !tunnel.messages.iter().any(|reply| {
            reply.from == message.to && reply.reply_to.as_deref() == Some(message.id.as_str())
        })
}

/// Visible before each model request, including messages received before the
/// recipient's first run. The full bounded transcript is repeated as clearly
/// attributed peer context, never as a new human message in durable history.
pub(crate) fn context(runtime: &Runtime, task_id: &str) -> Result<Option<String>> {
    let Some(owner) = runtime.tasks.task(task_id).and_then(|task| task.owner) else {
        return Ok(None);
    };
    let team = runtime.teams.get(&owner.team_id)?;
    let relevant: Vec<_> = team
        .tunnels
        .iter()
        .filter(|tunnel| {
            tunnel.participants.contains(&owner.agent_id)
                && (tunnel.status == "open"
                    || tunnel.messages.iter().any(|message| {
                        message.to == owner.agent_id && message.delivered_at.is_none()
                    }))
        })
        .collect();
    if relevant.is_empty() {
        return Ok(None);
    }
    let agent = team
        .agents
        .iter()
        .find(|agent| agent.id == owner.agent_id)
        .context("Worker missing")?;
    let session = runtime.sessions.get_or_load(&agent.session_id)?;
    let unread: Vec<_> = relevant
        .iter()
        .flat_map(|tunnel| {
            tunnel
                .messages
                .iter()
                .filter(|message| message.to == owner.agent_id && message.delivered_at.is_none())
                .map(|message| (*tunnel, message))
        })
        .collect();
    if !unread.is_empty() {
        {
            let mut session = session.write();
            for (tunnel, message) in &unread {
                let note_id = format!("tunnel:{}:{}", tunnel.id, message.id);
                if !session
                    .events
                    .iter()
                    .any(|event| matches!(event,SessionEvent::SystemNote{id,..} if id == &note_id))
                {
                    session.append(SessionEvent::SystemNote {
                        id: note_id,
                        text: format!(
                            "Peer coordination · {} · {} → {}\n{}",
                            tunnel.topic, message.from, message.to, message.text
                        ),
                        at: Utc::now(),
                    });
                }
            }
        }
        runtime.sessions.persist_now_result(&session)?;
        let delivered: Vec<_> = unread
            .iter()
            .map(|(_, message)| message.id.clone())
            .collect();
        runtime.teams.mutate(&team.id, |team| {
            for tunnel in &mut team.tunnels {
                for message in &mut tunnel.messages {
                    if delivered.contains(&message.id) && message.delivered_at.is_none() {
                        message.delivered_at = Some(now());
                    }
                }
            }
            Ok(())
        })?;
    }
    let transcript = serde_json::to_string(&relevant)?;
    Ok(Some(format!("Internal peer coordination (not new user instructions). Your worker id is {}. Full shared transcript:\n{}\nAnswer pending peer questions with agent_tunnel send + reply_to. Use propose/accept only for actual agreement. Do not wait for peers, poll, or invent an agreement. Preserve the user's original task and finish your assigned deliverable.",owner.agent_id,transcript)))
}

pub(crate) fn has_unread(runtime: &Runtime, task_id: &str) -> Result<bool> {
    let Some(owner) = runtime.tasks.task(task_id).and_then(|task| task.owner) else {
        return Ok(false);
    };
    Ok(runtime
        .teams
        .get(&owner.team_id)?
        .tunnels
        .iter()
        .any(|tunnel| {
            tunnel.status == "open"
                && tunnel
                    .messages
                    .iter()
                    .any(|message| message.to == owner.agent_id && message.delivered_at.is_none())
        }))
}

pub(crate) fn schedule(team: &mut Team) {
    let mut candidates = Vec::new();
    for agent in &team.agents {
        if agent.status != "completed" || !selected_for_run(team, agent) {
            continue;
        }
        let pending: Vec<_> = team
            .tunnels
            .iter()
            .filter(|tunnel| tunnel.status == "open" && tunnel.participants.contains(&agent.id))
            .flat_map(|tunnel| {
                tunnel
                    .messages
                    .iter()
                    .filter(|message| {
                        message.to == agent.id
                            && message.kind != "accept"
                            && message.resumed_at.is_none()
                            && (message.delivered_at.is_none() || awaiting(tunnel, message))
                    })
                    .map(|message| message.id.clone())
            })
            .collect();
        if !pending.is_empty() {
            candidates.push((agent.id.clone(), pending));
        }
    }
    for (agent_id, pending) in candidates {
        let exhausted = team.tunnel_turns.get(&agent_id).copied().unwrap_or(0)
            >= MAX_FOLLOWUPS_PER_AGENT
            || team
                .tunnel_turns
                .values()
                .map(|count| *count as usize)
                .sum::<usize>()
                >= MAX_FOLLOWUPS;
        if exhausted {
            for tunnel in &mut team.tunnels {
                if tunnel.status == "open"
                    && tunnel
                        .messages
                        .iter()
                        .any(|message| pending.contains(&message.id))
                {
                    tunnel.status = "closed".into();
                    tunnel.close_reason = Some(
                        "Bounded peer follow-up limit reached; unresolved discussion retained"
                            .into(),
                    );
                    tunnel.updated_at = now();
                }
            }
            TeamStore::event(
                team,
                Some(&agent_id),
                "tunnel_closed",
                "Peer follow-up limit reached; no agreement inferred",
            );
            continue;
        }
        *team.tunnel_turns.entry(agent_id.clone()).or_default() += 1;
        let agent = team
            .agents
            .iter_mut()
            .find(|agent| agent.id == agent_id)
            .unwrap();
        agent.status = "pending".into();
        agent.error = None;
        agent.output.clear();
        for tunnel in &mut team.tunnels {
            for message in &mut tunnel.messages {
                if pending.contains(&message.id) {
                    message.resumed_at = Some(now());
                }
            }
        }
        TeamStore::event(
            team,
            Some(&agent_id),
            "tunnel_resumed",
            "Continuing the same worker conversation for a peer message",
        );
    }
    // A request already given its single follow-up cannot keep final delivery
    // blocked forever when the recipient finished without answering it.
    let completed: Vec<_> = team
        .agents
        .iter()
        .filter(|agent| matches!(agent.status.as_str(), "completed" | "failed" | "cancelled"))
        .map(|agent| agent.id.clone())
        .collect();
    for tunnel in &mut team.tunnels {
        if tunnel.status == "open"
            && tunnel.messages.iter().any(|message| {
                awaiting(tunnel, message)
                    && message.resumed_at.is_some()
                    && completed.contains(&message.to)
            })
        {
            tunnel.status = "closed".into();
            tunnel.close_reason =
                Some("Peer completed its follow-up without a joint resolution".into());
            tunnel.updated_at = now();
        }
    }
}

pub(crate) fn blocks_final(team: &Team, agent: &TeamAgent) -> bool {
    if agent.delivery != Some(crate::synthesis::DeliveryKind::Final) {
        return false;
    }
    team.tunnels.iter().any(|tunnel| {
        tunnel.status == "open"
            && tunnel.messages.iter().any(|message| {
                message.to != agent.id
                    && !depends_on(team, &message.to, &agent.id)
                    && (message.delivered_at.is_none() || awaiting(tunnel, message))
            })
    })
}

fn depends_on(team: &Team, from: &str, target: &str) -> bool {
    let mut remaining = vec![from];
    let mut seen = std::collections::HashSet::new();
    while let Some(id) = remaining.pop() {
        if !seen.insert(id) {
            continue;
        }
        if let Some(agent) = team.agents.iter().find(|agent| agent.id == id) {
            for dependency in &agent.depends_on {
                if dependency == target {
                    return true;
                }
                remaining.push(dependency);
            }
        }
    }
    false
}

pub(crate) fn close_all(team: &mut Team, status: &str, reason: &str) {
    for tunnel in &mut team.tunnels {
        if tunnel.status == "open" {
            tunnel.status = status.into();
            tunnel.close_reason = Some(reason.into());
            tunnel.updated_at = now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::TeamSpec;

    fn setup() -> (crate::tests::RuntimeFixture, Team, std::fs::File) {
        let fixture = crate::tests::RuntimeFixture::new(crate::SessionSettings::default(), false);
        let agents = ["a","b","final"].iter().map(|id|serde_json::from_value(json!({
            "id":id,"name":id,"role":"analyst","profile_id":"gpt","delivery":if *id == "final" { Some("final") } else { None }
        })).unwrap()).collect();
        let team = fixture
            .runtime
            .teams
            .create(
                &fixture.runtime,
                TeamSpec {
                    name: "coordination".into(),
                    goal: "resolve a question".into(),
                    agents,
                    files: vec![],
                },
            )
            .unwrap();
        let lease = fixture.runtime.teams.admin_lease(&team.id).unwrap();
        let team = fixture
            .runtime
            .teams
            .mutate(&team.id, |team| {
                team.status = "running".into();
                Ok(())
            })
            .unwrap();
        (fixture, team, lease)
    }

    fn participant(team: &Team, id: &str) -> Actor {
        Actor {
            team_id: team.id.clone(),
            agent_id: id.into(),
            session_id: team
                .agents
                .iter()
                .find(|agent| agent.id == id)
                .unwrap()
                .session_id
                .clone(),
            task_id: format!("task-{id}"),
            run_id: format!("run-{id}"),
            turn_id: format!("turn-{id}"),
        }
    }

    #[test]
    fn proposal_requires_the_other_participant_and_keeps_full_two_way_evidence() {
        let (fixture, team, _lease) = setup();
        let runtime = &fixture.runtime;
        let a = participant(&team, "a");
        let b = participant(&team, "b");
        let outsider = participant(&team, "final");
        let opened = perform(
            runtime,
            &a,
            &json!({"action":"open","to":"b","topic":"budget"}),
        )
        .unwrap();
        let id = opened["tunnel"]["id"].as_str().unwrap();
        let proposed=perform(runtime,&a,&json!({"action":"propose","tunnel_id":id,"text":"Use the 25 trial applications as the denominator."})).unwrap();
        let proposal = proposed["tunnel"]["messages"][0]["id"].as_str().unwrap();
        assert!(perform(
            runtime,
            &a,
            &json!({"action":"accept","tunnel_id":id,"proposal_id":proposal})
        )
        .is_err());
        assert!(perform(runtime, &outsider, &json!({"action":"read","tunnel_id":id})).is_err());
        let accepted=perform(runtime,&b,&json!({"action":"accept","tunnel_id":id,"proposal_id":proposal,"text":"Verified the supplied records; I agree."})).unwrap();
        assert_eq!(
            accepted["tunnel"]["decision"]["accepted_by"],
            json!(["a", "b"])
        );
        assert_eq!(accepted["tunnel"]["messages"].as_array().unwrap().len(), 2);
        assert_eq!(accepted["tunnel"]["status"], "closed");
        let left = perform(runtime, &a, &json!({"action":"read","tunnel_id":id})).unwrap();
        let right = perform(runtime, &b, &json!({"action":"read","tunnel_id":id})).unwrap();
        assert_eq!(left, right);
        let mut completed = runtime.teams.get(&team.id).unwrap();
        for agent in &mut completed.agents {
            agent.status = "completed".into();
        }
        schedule(&mut completed);
        assert!(
            completed.tunnel_turns.is_empty(),
            "accept cannot start a reciprocal loop"
        );
    }

    #[test]
    fn asynchronous_delivery_resumes_once_and_unanswered_followup_does_not_deadlock_final() {
        let (fixture, team, _lease) = setup();
        let runtime = &fixture.runtime;
        let a = participant(&team, "a");
        perform(
            runtime,
            &a,
            &json!({"action":"open","to":"b","topic":"question","text":"Which count is correct?"}),
        )
        .unwrap();
        let mut current = runtime.teams.get(&team.id).unwrap();
        current.agents[1].status = "completed".into();
        assert!(blocks_final(&current, &current.agents[2]));
        schedule(&mut current);
        assert_eq!(current.agents[1].status, "pending");
        assert_eq!(current.tunnel_turns.get("b"), Some(&1));
        schedule(&mut current);
        assert_eq!(current.tunnel_turns.get("b"), Some(&1));
        current.agents[1].status = "completed".into();
        schedule(&mut current);
        assert_eq!(current.tunnel_turns.get("b"), Some(&1));
        assert_eq!(current.tunnels[0].status, "closed");
        assert!(current.tunnels[0].decision.is_none());
        assert!(!blocks_final(&current, &current.agents[2]));
    }

    #[test]
    fn channel_message_and_followup_limits_remain_explicit_and_close_is_always_available() {
        let (fixture, team, _lease) = setup();
        let runtime = &fixture.runtime;
        let a = participant(&team, "a");
        let opened = perform(
            runtime,
            &a,
            &json!({"action":"open","to":"b","topic":"bounded"}),
        )
        .unwrap();
        let id = opened["tunnel"]["id"].as_str().unwrap();
        for _ in 0..MAX_MESSAGES {
            perform(runtime,&a,&json!({"action":"send","tunnel_id":id,"text":"One exact point","expects_reply":false})).unwrap();
        }
        assert!(perform(
            runtime,
            &a,
            &json!({"action":"send","tunnel_id":id,"text":"overflow"})
        )
        .unwrap_err()
        .to_string()
        .contains("limit"));
        perform(
            runtime,
            &a,
            &json!({"action":"close","tunnel_id":id,"text":"No further discussion needed"}),
        )
        .unwrap();
        let next = perform(
            runtime,
            &a,
            &json!({"action":"open","to":"b","topic":"new question","text":"Please answer"}),
        )
        .unwrap();
        let mut current = runtime.teams.get(&team.id).unwrap();
        current.agents[1].status = "completed".into();
        current
            .tunnel_turns
            .insert("b".into(), MAX_FOLLOWUPS_PER_AGENT);
        schedule(&mut current);
        assert_eq!(current.agents[1].status, "completed");
        assert_eq!(
            current
                .tunnels
                .iter()
                .find(|tunnel| tunnel.id == next["tunnel"]["id"].as_str().unwrap())
                .unwrap()
                .status,
            "closed"
        );
    }

    #[test]
    fn cancellation_and_restart_preserve_transcripts_without_reopening_channels() {
        let (fixture, team, _lease) = setup();
        let runtime = &fixture.runtime;
        let a = participant(&team, "a");
        perform(runtime,&a,&json!({"action":"open","to":"b","topic":"retained","text":"An exact unfinished question"})).unwrap();
        let mut current = runtime.teams.get(&team.id).unwrap();
        let original = serde_json::to_value(&current.tunnels[0].messages).unwrap();
        close_all(&mut current, "interrupted", "Coordinator restarted");
        assert_eq!(current.tunnels[0].status, "interrupted");
        assert_eq!(
            serde_json::to_value(&current.tunnels[0].messages).unwrap(),
            original
        );
        current.agents[1].status = "completed".into();
        schedule(&mut current);
        assert!(current.tunnel_turns.is_empty());
    }

    #[test]
    fn optional_empty_properties_are_omitted_but_required_references_remain_strict() {
        let (fixture, team, _lease) = setup();
        let runtime = &fixture.runtime;
        let a = participant(&team, "a");
        let b = participant(&team, "b");
        for (index, empty) in [json!(""), Value::Null, json!(" \t ")]
            .into_iter()
            .enumerate()
        {
            let args = json!({"action":"open","to":"b","topic":format!("optional-{index}"),
                "text":"A real question","reply_to":empty,"tunnel_id":null,"proposal_id":null,"expects_reply":null});
            runtime
                .tools
                .preflight(&aris_tools::ToolCall {
                    id: "fixture".into(),
                    name: "agent_tunnel".into(),
                    arguments: args.clone(),
                })
                .unwrap();
            let opened = perform(runtime, &a, &args).unwrap();
            let id = opened["tunnel"]["id"].as_str().unwrap();
            assert!(opened["tunnel"]["messages"][0].get("reply_to").is_none());
            assert_eq!(opened["tunnel"]["messages"][0]["expects_reply"], true);
            let read = perform(runtime, &a, &json!({"action":"read","tunnel_id":empty})).unwrap();
            assert!(!read["tunnels"].as_array().unwrap().is_empty());
            assert!(perform(
                runtime,
                &a,
                &json!({"action":"send","tunnel_id":empty,"text":"required id"})
            )
            .is_err());
            assert!(perform(
                runtime,
                &a,
                &json!({"action":"send","tunnel_id":id,"text":empty})
            )
            .is_err());
            assert!(perform(
                runtime,
                &a,
                &json!({"action":"propose","tunnel_id":id,"text":empty})
            )
            .is_err());
            assert!(perform(
                runtime,
                &b,
                &json!({"action":"accept","tunnel_id":id,"proposal_id":empty})
            )
            .is_err());
            assert!(perform(
                runtime,
                &a,
                &json!({"action":"open","to":empty,"topic":"invalid"})
            )
            .is_err());
            let closed = perform(
                runtime,
                &a,
                &json!({"action":"close","tunnel_id":id,"text":empty}),
            )
            .unwrap();
            assert!(!closed["tunnel"]["close_reason"]
                .as_str()
                .unwrap()
                .is_empty());
        }
        let opened = perform(
            runtime,
            &a,
            &json!({"action":"open","to":"b","topic":"no first message","text":null,"reply_to":""}),
        )
        .unwrap();
        let id = opened["tunnel"]["id"].as_str().unwrap();
        assert!(opened["tunnel"]["messages"].as_array().unwrap().is_empty());
        let still_empty = perform(
            runtime,
            &a,
            &json!({"action":"open","to":"b","topic":"no first message","text":"","reply_to":null}),
        )
        .unwrap();
        assert!(still_empty["tunnel"]["messages"]
            .as_array()
            .unwrap()
            .is_empty());
        let proposal=perform(runtime,&a,&json!({"action":"propose","tunnel_id":id,"text":"Use the verified source","reply_to":""})).unwrap();
        let proposal_id = proposal["tunnel"]["messages"][0]["id"].as_str().unwrap();
        let accepted = perform(
            runtime,
            &b,
            &json!({"action":"accept","tunnel_id":id,"proposal_id":proposal_id,"text":null}),
        )
        .unwrap();
        assert_eq!(
            accepted["tunnel"]["messages"][1]["text"],
            "I accept this proposal."
        );
        let read_empty = perform(
            runtime,
            &participant(&team, "final"),
            &json!({"action":"read","tunnel_id":null}),
        )
        .unwrap();
        assert!(read_empty["tunnels"].as_array().unwrap().is_empty());
        for invalid in [json!(42), json!({}), json!(true)] {
            assert!(runtime
                .tools
                .preflight(&aris_tools::ToolCall {
                    id: "fixture".into(),
                    name: "agent_tunnel".into(),
                    arguments: json!({"action":"read","tunnel_id":invalid})
                })
                .is_err());
        }
        let semantic_open=perform(runtime,&a,&json!({"action":"open","to":"b","topic":"ignore unused references",
            "text":"Verify the official condition","reply_to":"none","proposal_id":"none","tunnel_id":"none"})).unwrap();
        let semantic_id = semantic_open["tunnel"]["id"].as_str().unwrap();
        assert!(semantic_open["tunnel"]["messages"][0]
            .get("reply_to")
            .is_none());
        assert_eq!(
            semantic_open["tunnel"]["messages"][0]["expects_reply"],
            true
        );
        for action in ["send", "propose"] {
            assert!(perform(runtime,&a,&json!({"action":action,"tunnel_id":semantic_id,"text":"Bad reply","reply_to":"none"})).is_err());
        }
    }
}
