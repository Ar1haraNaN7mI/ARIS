//! Bounded, nonblocking peer coordination between registered native workers.
//! Team.tunnels is the durable source of truth; tools never wait for another
//! agent or start a second competing runner for a live conversation.
mod wire;
pub(crate) use wire::model_transcript;

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
const MAX_ACTIVE_PER_AGENT: usize = 2;
const MAX_MESSAGES: usize = 16;
const MAX_FOLLOWUPS_PER_AGENT: u8 = 2;
pub(crate) const MAX_FOLLOWUPS: usize = 8;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelMessage {
    pub id: String,
    pub seq: usize,
    pub from: String,
    pub to: String,
    pub text: String,
    /// Native JSON shared with the peer. Text remains the human transcript.
    #[serde(
        flatten,
        serialize_with = "wire::serialize_data",
        deserialize_with = "wire::deserialize_data"
    )]
    pub data: Option<Value>,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposal_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    pub expects_reply: bool,
    /// The requester needs to continue its own assignment after the peer's result.
    #[serde(default)]
    pub resume_on_reply: bool,
    pub at: String,
    pub session_id: String,
    pub task_id: String,
    pub run_id: String,
    pub turn_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumed_at: Option<String>,
    /// Co-work evidence: distinguish an invite to an idle peer from a request
    /// received in flight, and admission to a follow-up from its actual start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipient_status_at_send: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_as: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub followup_closed_reason: Option<String>,
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
    body: wire::Body,
    reply_to: Option<String>,
    resume_on_reply: bool,
) -> Result<()> {
    ensure!(
        tunnel.messages.len() < MAX_MESSAGES,
        "Tunnel message limit reached; close it with the unresolved result"
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
    let expects_reply = matches!(kind, "question" | "proposal" | "cowork_invite");
    tunnel.messages.push(TunnelMessage {
        id: id(),
        seq: tunnel.messages.len() + 1,
        from: actor.agent_id.clone(),
        to: peer,
        text: body.text,
        data: body.data,
        kind: kind.into(),
        proposal_id: if kind == "accept" {
            reply_to.clone()
        } else {
            None
        },
        reply_to,
        expects_reply,
        resume_on_reply: expects_reply && resume_on_reply,
        at: at.clone(),
        session_id: actor.session_id.clone(),
        task_id: actor.task_id.clone(),
        run_id: actor.run_id.clone(),
        turn_id: actor.turn_id.clone(),
        delivered_at: None,
        resumed_at: None,
        recipient_status_at_send: None,
        started_at: None,
        started_as: None,
        followup_closed_reason: None,
    });
    tunnel.updated_at = at;
    Ok(())
}

fn respond_to_invite(
    tunnel: &mut AgentTunnel,
    actor: &Actor,
    action: &str,
    args: &Value,
) -> Result<()> {
    let invite = text(args, "reply_to")?;
    ensure!(
        tunnel
            .messages
            .iter()
            .any(|m| m.id == invite && m.kind == "cowork_invite" && m.to == actor.agent_id),
        "reply_to must identify a co-work invitation received from this peer"
    );
    let previous = tunnel
        .messages
        .iter()
        .rev()
        .find(|m| {
            m.reply_to.as_deref() == Some(invite)
                && matches!(
                    m.kind.as_str(),
                    "cowork_accept" | "cowork_decline" | "cowork_result"
                )
        })
        .map(|m| m.kind.as_str());
    let (kind, default_text) = match action {
        "accept_work" => {
            ensure!(previous.is_none(), "This invitation already has a response");
            ("cowork_accept", Some("I will work on this task."))
        }
        "decline_work" => {
            ensure!(
                previous.is_none() || previous == Some("cowork_accept"),
                "This invitation is already settled"
            );
            ("cowork_decline", None)
        }
        "complete_work" => {
            ensure!(
                previous == Some("cowork_accept"),
                "Accept this invitation before delivering the result"
            );
            ("cowork_result", None)
        }
        _ => unreachable!(),
    };
    append(
        tunnel,
        actor,
        kind,
        wire::body(args, kind, default_text)?,
        Some(invite.into()),
        false,
    )
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
            .map(model_transcript)
            .collect();
        ensure!(
            !tunnels.is_empty() || requested_id.is_none(),
            "Tunnel not found for this worker"
        );
        return Ok(json!({"tunnels":tunnels,"blocking":false}));
    }
    let mut selected_id = None;
    let mut wake = false;
    let mut appended = None;
    let team = runtime.teams.mutate(&actor.team_id, |team| {
        ensure!(
            team.status == "running",
            "Team is stopping; coordination is closed"
        );
        let address_peer = action == "open"
            || (matches!(action, "send" | "ask" | "invite")
                && optional_text(args, "tunnel_id").is_none());
        let index = if address_peer {
            let to = text(args, "to")?;
            let topic = text(args, "topic")?;
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
        let was_active = has_pending_work(tunnel);
        let before = tunnel.messages.len();
        let reply_to = if address_peer && action != "send" {
            None
        } else {
            optional_text(args, "reply_to").map(str::to_owned)
        };
        match action {
            "open" | "send" | "ask" | "invite" | "propose" => {
                if action != "open"
                    || optional_text(args, "text").is_some()
                    || wire::data(args).is_some()
                {
                    let kind = match action {
                        "ask" => "question",
                        "propose" => "proposal",
                        "invite" => "cowork_invite",
                        _ if args.get("expects_reply").and_then(Value::as_bool) == Some(true) => {
                            "question"
                        }
                        _ if reply_to.as_ref().is_some_and(|source| {
                            tunnel.messages.iter().any(|m| {
                                m.id == *source && m.kind != "cowork_invite" && awaiting(tunnel, m)
                            })
                        }) =>
                        {
                            "answer"
                        }
                        _ => "notice",
                    };
                    append(
                        tunnel,
                        actor,
                        kind,
                        wire::body(args, kind, None)?,
                        reply_to,
                        args.get("resume_on_reply").and_then(Value::as_bool) == Some(true),
                    )?;
                }
            }
            "accept_work" | "decline_work" | "complete_work" => {
                respond_to_invite(tunnel, actor, action, args)?
            }
            "accept" => {
                ensure!(
                    tunnel.decision.is_none(),
                    "This tunnel already has a joint decision"
                );
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
                    wire::body(args, "accept", Some("I accept this proposal."))?,
                    Some(proposal.id.clone()),
                    false,
                )?;
                tunnel.decision = Some(TunnelDecision {
                    proposal_id: proposal.id,
                    text: proposal.text,
                    accepted_by: vec![proposal.from, actor.agent_id.clone()],
                    at: now(),
                });
                // A decision can share its transcript with unfinished co-work or
                // an expressly requested return to the proposer.
                if !has_pending_work(tunnel) {
                    tunnel.status = "closed".into();
                    tunnel.close_reason = Some("Both participants accepted the proposal".into());
                }
            }
            "close" => {
                ensure!(
                    wire::data(args).is_none(),
                    "close accepts a text reason; send data before closing"
                );
                let reason = optional_text(args, "text")
                    .unwrap_or("Closed by a participant without a joint decision");
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
        if let Some(message) = tunnel.messages.get(before) {
            appended = Some(before);
            wake = needs_attention(tunnel, message);
            if message.kind == "cowork_invite" {
                tunnel.messages[before].recipient_status_at_send = team
                    .agents
                    .iter()
                    .find(|agent| agent.id == message.to)
                    .map(|agent| agent.status.clone());
            }
        }
        if !was_active && has_pending_work(tunnel) {
            let participants = tunnel.participants.clone();
            for participant in &participants {
                let active = team
                    .tunnels
                    .iter()
                    .filter(|other| {
                        other.participants.contains(participant) && has_pending_work(other)
                    })
                    .count();
                ensure!(
                    active <= MAX_ACTIVE_PER_AGENT,
                    "{participant} already has two tunnels requiring a response or co-work"
                );
            }
        }
        TeamStore::event(team, Some(&actor.agent_id), event_kind, &summary);
        Ok(())
    })?;
    if wake {
        runtime.teams.wake_coordinator(&actor.team_id);
    }
    let tunnel = team
        .tunnels
        .iter()
        .find(|tunnel| Some(&tunnel.id) == selected_id.as_ref())
        .unwrap();
    Ok(wire::receipt(tunnel, appended))
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
            "Coordinate asynchronously via compact JSON. Send parameters, facts and artifact references in data; use text for an exact question, reason or wording. Address send/ask/invite by to+topic or tunnel_id. open/send notify; ask requests an answer; invite requests work. Set resume_on_reply when your next step needs the return. Answer with send+reply_to. For co-work use accept_work then complete_work, or decline_work, with reply_to=invite ID. propose/accept records agreement via proposal_id. Writes return message IDs; read retrieves the full exchange. Up to two outstanding channels per worker. Continue without polling.",
            json!({"type":"object","properties":{
                "action":{"type":"string","enum":["open","send","ask","invite","accept_work","decline_work","complete_work","read","propose","accept","close"]},
                "to":{"anyOf":[{"type":"string"},{"type":"null"}],"description":"Exact peer worker id. Pair with topic to open a tunnel or send directly."},
                "topic":{"anyOf":[{"type":"string"},{"type":"null"}],"description":"Topic for the peer route. Reusing the same open topic reuses its transcript."},
                "tunnel_id":{"anyOf":[{"type":"string"},{"type":"null"}],"description":"For send/ask/invite use an existing tunnel id or address via to and topic."},
                "text":{"anyOf":[{"type":"string"},{"type":"null"}]},
                "data":{"anyOf":[{"type":"object"},{"type":"array","items":{}},{"type":"string"},{"type":"number"},{"type":"boolean"},{"type":"null"}],"description":"Native JSON facts, parameters, sources or artifact paths. May replace or accompany text. Omit when unused."},
                "reply_to":{"anyOf":[{"type":"string"},{"type":"null"}]},
                "proposal_id":{"anyOf":[{"type":"string"},{"type":"null"}]},
                "expects_reply":{"anyOf":[{"type":"boolean"},{"type":"null"},{"enum":[""]}],"description":"For open/send, true requests an answer. Omit for notices and receipts."},
                "resume_on_reply":{"anyOf":[{"type":"boolean"},{"type":"null"},{"enum":[""]}],"description":"Only on a request whose answer or work result is needed to continue your own assignment."}
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

fn terminal_reply<'a>(
    tunnel: &'a AgentTunnel,
    request: &TunnelMessage,
) -> Option<&'a TunnelMessage> {
    tunnel.messages.iter().find(|reply| {
        reply.from == request.to
            && reply.reply_to.as_deref() == Some(request.id.as_str())
            && if request.kind == "cowork_invite" {
                matches!(reply.kind.as_str(), "cowork_result" | "cowork_decline")
            } else {
                matches!(reply.kind.as_str(), "answer" | "accept" | "message")
            }
    })
}

fn awaiting(tunnel: &AgentTunnel, message: &TunnelMessage) -> bool {
    tunnel.status == "open" && message.expects_reply && terminal_reply(tunnel, message).is_none()
}

/// Visibility and scheduling are separate. A receipt can continue the sender
/// only when they subscribed to the result of this particular request.
fn needs_attention(tunnel: &AgentTunnel, message: &TunnelMessage) -> bool {
    if message.followup_closed_reason.is_some() {
        return false;
    }
    awaiting(tunnel, message)
        || (tunnel.status == "open"
            && message.delivered_at.is_none()
            && message.reply_to.as_ref().is_some_and(|source| {
                tunnel.messages.iter().any(|request| {
                    request.id == *source
                        && request.expects_reply
                        && request.resume_on_reply
                        && terminal_reply(tunnel, request)
                            .is_some_and(|reply| reply.id == message.id)
                })
            }))
}

fn has_pending_work(tunnel: &AgentTunnel) -> bool {
    tunnel
        .messages
        .iter()
        .any(|message| needs_attention(tunnel, message))
}

/// Ending an unattended request must leave the opposite participant's ongoing work usable.
fn end_followups(tunnel: &mut AgentTunnel, messages: &[String], reason: &str) {
    if messages.is_empty() || tunnel.status != "open" {
        return;
    }
    let mut changed = false;
    for message in &mut tunnel.messages {
        if messages.contains(&message.id) {
            message.followup_closed_reason = Some(reason.into());
            changed = true;
        }
    }
    if changed {
        tunnel.updated_at = now();
        if !has_pending_work(tunnel) {
            tunnel.status = "closed".into();
            tunnel.close_reason = Some(reason.into());
        }
    }
}

/// Project peer input missing from this request's actual, budgeted history.
/// The durable transcript remains available to either participant through read.
pub(crate) fn context(
    runtime: &Runtime,
    task_id: &str,
    history: &mut [aris_llm::ChatMessage],
) -> Result<Option<String>> {
    let Some(owner) = runtime.tasks.task(task_id).and_then(|task| task.owner) else {
        return Ok(None);
    };
    let team = runtime.teams.get(&owner.team_id)?;
    if !team
        .tunnels
        .iter()
        .any(|t| t.participants.contains(&owner.agent_id))
    {
        return Ok(None);
    }
    let agent = team
        .agents
        .iter()
        .find(|agent| agent.id == owner.agent_id)
        .context("Worker missing")?;
    let session = runtime.sessions.get_or_load(&agent.session_id)?;
    let turn_start = session.read().events.iter().rev().find_map(|e| match e {
        SessionEvent::TurnStart { at, .. } => Some(*at),
        _ => None,
    });
    let visible = wire::project_history(history);
    let relevant: Vec<_> = team
        .tunnels
        .iter()
        .filter(|tunnel| {
            tunnel.participants.contains(&owner.agent_id)
                && (tunnel.status == "open"
                    || tunnel.messages.iter().any(|m| {
                        m.to == owner.agent_id
                            && match &m.delivered_at {
                                None => true,
                                // A closed exchange still supplies its last reply throughout the
                                // receiving turn, including steps after a tool call.
                                Some(at) => turn_start.is_some_and(|start| {
                                    at.parse::<chrono::DateTime<Utc>>()
                                        .is_ok_and(|delivered| delivered >= start)
                                }),
                            }
                    }))
        })
        .collect();
    if relevant.is_empty() {
        return Ok(None);
    }
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
    let inbox: Vec<_> = relevant
        .into_iter()
        .filter_map(|tunnel| wire::inbox(tunnel, &owner.agent_id, &visible))
        .collect();
    Ok((!inbox.is_empty()).then(|| json!({"peer_inbox":inbox}).to_string()))
}

pub(crate) fn has_unread_request_or_result(runtime: &Runtime, task_id: &str) -> Result<bool> {
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
                && tunnel.messages.iter().any(|message| {
                    message.to == owner.agent_id
                        && message.delivered_at.is_none()
                        && needs_attention(tunnel, message)
                })
        }))
}

/// Record a coalesced set of invitations on the actual lane-start event. A busy
/// worker receiving its new assignment in-context has no second start event.
pub(crate) fn mark_started(
    team: &mut Team,
    agent_id: &str,
    at: chrono::DateTime<Utc>,
    cowork_only: bool,
) -> Vec<(String, i64)> {
    let mut measurements = Vec::new();
    for tunnel in &mut team.tunnels {
        if tunnel.status != "open" || !tunnel.participants.iter().any(|id| id == agent_id) {
            continue;
        }
        for index in 0..tunnel.messages.len() {
            let message = &tunnel.messages[index];
            if message.to == agent_id
                && message.kind == "cowork_invite"
                && message.started_at.is_none()
                && needs_attention(tunnel, message)
                && (message.delivered_at.is_none() || message.resumed_at.is_some())
            {
                if let Ok(sent) = chrono::DateTime::parse_from_rfc3339(&message.at) {
                    measurements
                        .push((message.id.clone(), (at - sent.to_utc()).num_milliseconds()));
                }
                tunnel.messages[index].started_at = Some(at.to_rfc3339());
                tunnel.messages[index].started_as =
                    Some(if cowork_only { "co_work" } else { "assignment" }.into());
            }
        }
    }
    measurements
}

pub(crate) fn pending_invite<'a>(team: &'a Team, agent_id: &str) -> Option<&'a TunnelMessage> {
    let return_ready = team.tunnels.iter().any(|t| {
        t.messages.iter().any(|m| {
            m.to == agent_id
                && m.reply_to.is_some()
                && m.resumed_at.is_none()
                && needs_attention(t, m)
        })
    });
    team.tunnels.iter().find_map(|tunnel| {
        tunnel.messages.iter().find(|message| {
            message.to == agent_id
                && message.kind == "cowork_invite"
                && (message.started_at.is_none()
                    || (return_ready && message.started_as.as_deref() == Some("co_work")))
                && needs_attention(tunnel, message)
        })
    })
}

pub(crate) fn has_followup_budget(team: &Team, peer: &str) -> bool {
    team.tunnel_turns.get(peer).copied().unwrap_or(0) < MAX_FOLLOWUPS_PER_AGENT
        && team
            .tunnel_turns
            .values()
            .map(|count| usize::from(*count))
            .sum::<usize>()
            < MAX_FOLLOWUPS
}

impl TunnelMessage {
    pub(crate) fn reserved_turn(&self) -> bool {
        self.resumed_at.is_some() && self.started_at.is_none()
    }
}

/// A pending primary role borrows its own lane to handle current invitations.
/// One coalesced side turn consumes one follow-up, including one that resumes
/// after the requester supplies missing information.
pub(crate) fn begin_side_turn(team: &mut Team, peer: &str) -> Vec<String> {
    let Some(invite) = pending_invite(team, peer) else {
        return Vec::new();
    };
    // schedule() already charged a completed role when it made it pending.
    let reserved = invite.reserved_turn();
    if !reserved && !has_followup_budget(team, peer) {
        return Vec::new();
    }
    let mut invites = Vec::new();
    let at = now();
    for tunnel in &mut team.tunnels {
        for i in 0..tunnel.messages.len() {
            let m = &tunnel.messages[i];
            if m.to != peer || !needs_attention(tunnel, m) {
                continue;
            }
            if m.kind == "cowork_invite" {
                invites.push(m.id.clone());
                tunnel.messages[i]
                    .resumed_at
                    .get_or_insert_with(|| at.clone());
            } else if m.reply_to.is_some() {
                tunnel.messages[i]
                    .resumed_at
                    .get_or_insert_with(|| at.clone());
            }
        }
    }
    if !reserved {
        *team.tunnel_turns.entry(peer.to_owned()).or_default() += 1;
    }
    TeamStore::event(
        team,
        Some(peer),
        "cowork_turn_started",
        "Handling peer work while the primary assignment awaits its inputs",
    );
    invites
}

pub(crate) fn finish_side_turn(
    team: &mut Team,
    peer: &str,
    invitations: &[String],
    error: Option<&str>,
) {
    let awaiting_input = team.tunnels.iter().any(|t| {
        t.messages.iter().any(|m| {
            (m.from == peer
                && m.kind == "question"
                && m.resume_on_reply
                && m.followup_closed_reason.is_none()
                && awaiting(t, m))
                || (m.to == peer
                    && m.reply_to.is_some()
                    && m.resumed_at.is_none()
                    && needs_attention(t, m))
        })
    });
    if error.is_some() || !awaiting_input {
        for tunnel in &mut team.tunnels {
            let unfinished = tunnel
                .messages
                .iter()
                .filter(|m| invitations.contains(&m.id) && awaiting(tunnel, m))
                .map(|m| m.id.clone())
                .collect::<Vec<_>>();
            end_followups(
                tunnel,
                &unfinished,
                error.unwrap_or("Co-work turn ended before returning a result"),
            );
        }
    }
    TeamStore::event(
        team,
        Some(peer),
        "cowork_turn_finished",
        error.unwrap_or("Co-work turn ended; the primary assignment remains pending"),
    );
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
                            && message.resumed_at.is_none()
                            && needs_attention(tunnel, message)
                    })
                    .map(|message| message.id.clone())
            })
            .collect();
        if !pending.is_empty() {
            candidates.push((agent.id.clone(), pending));
        }
    }
    for (agent_id, pending) in candidates {
        if !has_followup_budget(team, &agent_id) {
            for tunnel in &mut team.tunnels {
                end_followups(
                    tunnel,
                    &pending,
                    "Bounded peer follow-up limit reached; unresolved discussion retained",
                );
            }
            TeamStore::event(
                team,
                Some(&agent_id),
                "tunnel_followup_ended",
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
        let unanswered: Vec<_> = tunnel
            .messages
            .iter()
            .filter(|message| {
                message.followup_closed_reason.is_none()
                    && awaiting(tunnel, message)
                    && message.resumed_at.is_some()
                    && completed.contains(&message.to)
            })
            .map(|message| message.id.clone())
            .collect();
        end_followups(
            tunnel,
            &unanswered,
            "Peer completed its follow-up without a joint resolution",
        );
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
                    && needs_attention(tunnel, message)
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
        let agents = ["a","b","final","c"].iter().map(|id|serde_json::from_value(json!({
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

    fn read_channel(runtime: &Runtime, actor: &Actor, receipt: &Value) -> Value {
        perform(
            runtime,
            actor,
            &json!({"action":"read", "tunnel_id":receipt["tunnel"]["id"]}),
        )
        .unwrap()["tunnels"][0]
            .clone()
    }

    #[test]
    fn wide_peer_integers_render_exactly_without_duplicating_model_data() {
        let data = json!({"rows":[{"id":u64::MAX,"delta":-9_007_199_254_740_993_i64}]});
        let legacy = json!({"id":"message","seq":1,"from":"a","to":"b","text":"已核对。","kind":"notice",
            "expects_reply":false,"at":"now","session_id":"session","task_id":"task","run_id":"run",
            "turn_id":"turn","data":data});
        let message: TunnelMessage = serde_json::from_str(&legacy.to_string()).unwrap();
        let ui = serde_json::to_value(&message).unwrap();
        assert_eq!(ui["data"], data);
        assert_eq!(
            ui["data_json"],
            serde_json::to_string_pretty(&data).unwrap()
        );
        let mut cache = ui;
        cache["data_json"] = json!("rounded or stale");
        let reread: TunnelMessage = serde_json::from_value(cache).unwrap();
        assert_eq!(
            serde_json::to_value(&reread).unwrap()["data_json"],
            serde_json::to_string_pretty(&data).unwrap()
        );

        let (fixture, team, _lease) = setup();
        let sent = perform(
            &fixture.runtime,
            &participant(&team, "a"),
            &json!({"action":"send","to":"b","topic":"精度","data":data}),
        )
        .unwrap();
        let read = read_channel(&fixture.runtime, &participant(&team, "b"), &sent);
        assert_eq!(read["messages"][0]["data"], data);
        assert!(read["messages"][0].get("data_json").is_none());

        let mut safe = legacy;
        safe["data"] = json!([
            9_007_199_254_740_991_u64,
            -9_007_199_254_740_991_i64,
            0.5,
            "18446744073709551615"
        ]);
        let message: TunnelMessage = serde_json::from_value(safe.clone()).unwrap();
        let ui = serde_json::to_value(message).unwrap();
        assert_eq!(ui["data"], safe["data"]);
        assert!(ui.get("data_json").is_none());
        safe.as_object_mut().unwrap().remove("data");
        let historic: TunnelMessage = serde_json::from_value(safe).unwrap();
        let ui = serde_json::to_value(historic).unwrap();
        assert!(ui.get("data").is_none() && ui.get("data_json").is_none());
    }

    #[test]
    fn structured_cowork_moves_native_values_once_and_keeps_a_readable_lane() {
        let (fixture, team, _lease) = setup();
        let rt = &fixture.runtime;
        let a = participant(&team, "a");
        let b = participant(&team, "b");
        let inputs = json!({"summary":"核对辅料成本","unit":"元","rows":[[2,4.5,null],[3,0,false]],"source":"notes/原始表.tsv"});
        let args =
            json!({"action":"invite","to":"b","topic":"成本","data":inputs,"resume_on_reply":true});
        rt.tools
            .preflight(&aris_tools::ToolCall {
                id: "call".into(),
                name: "agent_tunnel".into(),
                arguments: args.clone(),
            })
            .unwrap();
        let sent = perform(rt, &a, &args).unwrap();
        let tunnel_id = sent["tunnel"]["id"].as_str().unwrap();
        let invite_id = sent["tunnel"]["messages"][0]["id"].as_str().unwrap();
        assert!(!sent.to_string().contains("原始表.tsv"));
        let read = read_channel(rt, &b, &sent);
        let first = &read["messages"][0];
        assert_eq!(first["id"], invite_id);
        assert_eq!(first["data"], inputs);
        assert!(first.get("text").is_none());
        assert_eq!(first["resume_on_reply"], true);
        assert!(first.get("session_id").is_none());
        let saved = rt.teams.get(&team.id).unwrap();
        assert_eq!(saved.tunnels[0].messages[0].text, "核对辅料成本");
        assert_eq!(saved.tunnels[0].messages[0].task_id, a.task_id);
        perform(
            rt,
            &b,
            &json!({"action":"accept_work","tunnel_id":tunnel_id,"reply_to":invite_id}),
        )
        .unwrap();
        let result = json!({"amount":9.0, "units":"元", "references":["notes/原始表.tsv"]});
        let receipt = perform(rt,&b,&json!({"action":"complete_work","tunnel_id":tunnel_id,"reply_to":invite_id,"text":"两笔合计九元。","data":result})).unwrap();
        assert_eq!(receipt["tunnel"]["messages"].as_array().unwrap().len(), 1);
        assert_eq!(receipt["tunnel"]["messages"][0]["kind"], "cowork_result");
        let full = read_channel(rt, &a, &sent);
        assert_eq!(read_channel(rt, &b, &sent), full);
        assert_eq!(full["messages"][2]["text"], "两笔合计九元。");
        assert_eq!(full["messages"][2]["data"], result);
        assert_eq!(full["messages"][2]["reply_to"], invite_id);
        assert!(perform(
            rt,
            &participant(&team, "c"),
            &json!({"action":"read","tunnel_id":tunnel_id})
        )
        .is_err());
    }

    #[test]
    fn projected_inbox_skips_visible_reads_but_recovers_after_history_eviction() {
        use aris_llm::{ChatMessage, FunctionCallDelta, ToolCallDelta};
        let (fixture, team, _lease) = setup();
        let a = participant(&team, "a");
        let b = participant(&team, "b");
        let opened = perform(
            &fixture.runtime,
            &a,
            &json!({"action":"send","to":"b","topic":"metrics","data":{"count":0}}),
        )
        .unwrap();
        let first_id = &opened["tunnel"]["messages"][0]["id"];
        let old = fixture
            .runtime
            .teams
            .get(&team.id)
            .unwrap()
            .tunnels
            .remove(0);
        let mut legacy = serde_json::to_value(&old).unwrap();
        legacy["messages"][0]
            .as_object_mut()
            .unwrap()
            .remove("data");
        legacy["messages"][0]["text"] = json!("Original wording");
        let mut call = ChatMessage::assistant("");
        call.tool_calls = Some(vec![ToolCallDelta {
            id: Some("read".into()),
            function: Some(FunctionCallDelta {
                name: Some("agent_tunnel".into()),
                arguments: Some(json!({"action":"read"}).to_string()),
            }),
            ..Default::default()
        }]);
        let raw = json!({"tunnels":[legacy],"blocking":false}).to_string();
        let mut history = vec![call, ChatMessage::tool_result("read", &raw)];
        let known = wire::project_history(&mut history);
        assert!(raw.contains("session_id") && !history[1].text().contains("session_id"));
        assert!(history[1].text().contains("Original wording"));
        assert!(wire::inbox(&old, &b.agent_id, &known).is_none());
        let reopened = wire::inbox(&old, &b.agent_id, &Default::default()).unwrap();
        assert_eq!(reopened["messages"][0]["id"], *first_id);
        let new = perform(
            &fixture.runtime,
            &a,
            &json!({"action":"send","tunnel_id":old.id,"data":false}),
        )
        .unwrap();
        let current = fixture
            .runtime
            .teams
            .get(&team.id)
            .unwrap()
            .tunnels
            .remove(0);
        let just_new = wire::inbox(&current, &b.agent_id, &known).unwrap();
        assert_eq!(just_new["messages"].as_array().unwrap().len(), 1);
        assert_eq!(just_new["messages"][0]["data"], false);
        assert_eq!(
            just_new["messages"][0]["id"],
            new["tunnel"]["messages"][0]["id"]
        );
        let indexed = ChatMessage::user(
            json!({"type":"aris.brief","tunnels":[{"id":old.id,"content_path":"trace.json"}]})
                .to_string(),
        );
        assert!(wire::project_history(&mut [indexed]).is_empty());
        let inline = ChatMessage::user(
            json!({"type":"aris.brief","tunnels":[model_transcript(&current)]}).to_string(),
        );
        assert!(
            wire::inbox(&current, &b.agent_id, &wire::project_history(&mut [inline])).is_none()
        );
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
        let transcript = read_channel(runtime, &a, &accepted);
        assert_eq!(transcript["decision"]["accepted_by"], json!(["a", "b"]));
        assert_eq!(transcript["messages"].as_array().unwrap().len(), 2);
        assert_eq!(accepted["tunnel"]["messages"].as_array().unwrap().len(), 1);
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
    fn both_participants_read_complete_long_messages_after_reopen() {
        let (fixture, team, _lease) = setup();
        let runtime = &fixture.runtime;
        let a = participant(&team, "a");
        let b = participant(&team, "b");
        let topic = "预算复核".repeat(100);
        let body = format!("{}\nEND-OF-EVIDENCE", "上游数据🧭\n".repeat(10_000));
        let opened = perform(
            runtime,
            &a,
            &json!({"action":"open","to":"b","topic":topic,"text":body}),
        )
        .unwrap();
        let read = json!({"action":"read","tunnel_id":opened["tunnel"]["id"]});
        for actor in [&a, &b] {
            let thread = perform(runtime, actor, &read).unwrap();
            assert_eq!(thread["tunnels"][0]["topic"], topic);
            assert_eq!(thread["tunnels"][0]["messages"][0]["text"], body);
        }
    }

    #[test]
    fn unread_notice_is_visible_to_both_peers_without_waking_either() {
        let (fixture, team, _lease) = setup();
        let a = participant(&team, "a");
        let b = participant(&team, "b");
        let runtime = &fixture.runtime;
        let opened = perform(
            runtime,
            &a,
            &json!({
                "action":"open","to":"b","topic":"审批口径","text":"已采纳预算测算。"
            }),
        )
        .unwrap();
        let read = json!({"action":"read","tunnel_id":opened["tunnel"]["id"]});
        assert_eq!(
            perform(runtime, &a, &read).unwrap(),
            perform(runtime, &b, &read).unwrap()
        );
        let mut current = runtime.teams.get(&team.id).unwrap();
        let message = &current.tunnels[0].messages[0];
        assert_eq!(message.kind, "notice");
        assert!(message.delivered_at.is_none());
        current.agents[1].status = "completed".into();
        current.agents[1].output = "original financial analysis".into();
        schedule(&mut current);
        assert!(current.tunnel_turns.is_empty());
        assert_eq!(current.agents[1].output, "original financial analysis");
        assert!(!blocks_final(&current, &current.agents[2]));

        let question = perform(
            runtime,
            &a,
            &json!({
                "action":"ask","tunnel_id":opened["tunnel"]["id"],"text":"请给出净额。"
            }),
        )
        .unwrap();
        let answer = perform(
            runtime,
            &b,
            &json!({
                "action":"send","tunnel_id":opened["tunnel"]["id"],
                "reply_to":question["tunnel"]["messages"][0]["id"],"text":"净额 500 元。"
            }),
        )
        .unwrap();
        assert_eq!(answer["tunnel"]["messages"][0]["kind"], "answer");
        let receipt = perform(
            runtime,
            &a,
            &json!({
                "action":"send","tunnel_id":opened["tunnel"]["id"],
                "reply_to":answer["tunnel"]["messages"][0]["id"],"text":"已采纳。"
            }),
        )
        .unwrap();
        assert_eq!(receipt["tunnel"]["messages"][0]["kind"], "notice");
        let mut current = runtime.teams.get(&team.id).unwrap();
        current
            .agents
            .iter_mut()
            .for_each(|a| a.status = "completed".into());
        schedule(&mut current);
        assert!(current.tunnel_turns.is_empty());
    }

    #[test]
    fn notices_reuse_peer_routes_without_taking_active_request_slots() {
        let (fixture, team, _lease) = setup();
        let runtime = &fixture.runtime;
        let final_owner = participant(&team, "final");
        let post = |from: &str, action: &str, topic: &str| {
            perform(
                runtime,
                &participant(&team, from),
                &json!({
                    "action":action, "to":"final", "topic":topic, "text":"Verify the source totals"
                }),
            )
        };
        let question = post("a", "ask", "a").unwrap();
        post("b", "invite", "b").unwrap();
        // Three distinct upstream roles can notify a writer already handling two requests.
        for source in ["a", "b", "c"] {
            let posted = post(source, "send", source).unwrap();
            let transcript = read_channel(runtime, &final_owner, &posted);
            let last = transcript["messages"].as_array().unwrap().last().unwrap();
            assert_eq!(last["kind"], "notice");
            assert_eq!(last["from"], source);
            assert_eq!(last["to"], "final");
        }
        let before = runtime.teams.get(&team.id).unwrap();
        assert_eq!(before.tunnels.len(), 3);
        assert!(post("c", "ask", "new topic").is_err());
        let passive = before.tunnels.iter().find(|t| t.topic == "c").unwrap();
        let c = participant(&team, "c");
        for action in ["ask", "invite", "propose", "open", "send"] {
            let attempt = json!({
                "action":action, "to":"final", "topic":"c", "tunnel_id":passive.id,
                "text":"Need a decision", "expects_reply":true
            });
            assert!(perform(runtime, &c, &attempt)
                .unwrap_err()
                .to_string()
                .contains("two tunnels"));
        }
        let after = runtime.teams.get(&team.id).unwrap();
        assert_eq!(
            serde_json::to_value(&after.tunnels).unwrap(),
            serde_json::to_value(&before.tunnels).unwrap()
        );
        let reply = |id: &Value| {
            perform(
                runtime,
                &final_owner,
                &json!({
                    "action":"send","to":"a","topic":"a","reply_to":id,"text":"Verified 12 transactions."
                }),
            )
        };
        assert!(
            reply(&json!(passive.messages[0].id)).is_err(),
            "a direct reply must reference a message in its actual route"
        );
        let answer = reply(&question["tunnel"]["messages"][0]["id"]).unwrap();
        assert_eq!(answer["tunnel"]["messages"][0]["kind"], "answer");
        assert_eq!(
            read_channel(runtime, &final_owner, &answer)["messages"][2]["reply_to"],
            question["tunnel"]["messages"][0]["id"]
        );
        post("c", "ask", "c").unwrap();
        let current = runtime.teams.get(&team.id).unwrap();
        assert_eq!(current.tunnels.len(), 3);
        assert_eq!(
            current
                .tunnels
                .iter()
                .filter(|t| has_pending_work(t))
                .count(),
            2
        );
        for index in current.tunnels.len()..MAX_TUNNELS {
            post("a", "send", &format!("notice {index}")).unwrap();
        }
        assert!(post("a", "send", "extra notice")
            .unwrap_err()
            .to_string()
            .contains("8-tunnel limit"));
    }

    #[test]
    fn cowork_tracks_the_invited_workers_acceptance_and_result() {
        for continue_with_result in [false, true] {
            let (fixture, team, _lease) = setup();
            let runtime = &fixture.runtime;
            let a = participant(&team, "a");
            let b = participant(&team, "b");
            let opened = perform(runtime, &a, &json!({
                "action":"invite","to":"b","topic":"现金预算","text":"用历史数据复算预算并返回公式",
                "resume_on_reply":continue_with_result
            })).unwrap();
            let channel = &opened["tunnel"]["id"];
            let invitation = &opened["tunnel"]["messages"][0]["id"];
            let respond = |who: &Actor, action: &str| {
                perform(
                    runtime,
                    who,
                    &json!({
                        "action":action, "tunnel_id":channel, "reply_to":invitation, "text":"1200 × 2 = 2400"
                    }),
                )
            };
            let mut current = runtime.teams.get(&team.id).unwrap();
            current.agents[1].status = "completed".into();
            schedule(&mut current);
            assert_eq!(current.tunnel_turns.get("b"), Some(&1));
            assert!(respond(&a, "accept_work").is_err());
            assert!(respond(&b, "complete_work").is_err());
            respond(&b, "accept_work").unwrap();
            assert!(respond(&b, "accept_work").is_err());
            let current = runtime.teams.get(&team.id).unwrap();
            assert!(blocks_final(&current, &current.agents[2]));
            assert!(!needs_attention(
                &current.tunnels[0],
                &current.tunnels[0].messages[1]
            ));
            respond(&b, "complete_work").unwrap();
            assert!(respond(&b, "complete_work").is_err());
            assert!(respond(&b, "decline_work").is_err());
            let mut current = runtime.teams.get(&team.id).unwrap();
            let result = &current.tunnels[0].messages[2];
            assert_eq!(result.kind, "cowork_result");
            assert_eq!(result.task_id, b.task_id);
            assert_eq!(result.reply_to.as_deref(), invitation.as_str());
            assert_eq!(result.text, "1200 × 2 = 2400");
            current.agents[0].status = "completed".into();
            current.agents[1].status = "completed".into();
            schedule(&mut current);
            assert_eq!(current.tunnel_turns.contains_key("a"), continue_with_result);
            assert!(!current.tunnel_turns.contains_key("b"));
        }
    }

    #[test]
    fn exhausted_request_leaves_reverse_cowork_open_for_its_result() {
        let (fixture, team, _lease) = setup();
        let runtime = &fixture.runtime;
        let a = participant(&team, "a");
        let b = participant(&team, "b");
        let asked = perform(
            runtime,
            &a,
            &json!({"action":"ask","to":"b","topic":"review","text":"Confirm the time window"}),
        )
        .unwrap();
        let tunnel = &asked["tunnel"]["id"];
        let invited = perform(runtime,&b,&json!({"action":"invite","tunnel_id":tunnel,"text":"Cross-check the total independently"})).unwrap();
        let invite = &invited["tunnel"]["messages"][0]["id"];
        perform(
            runtime,
            &a,
            &json!({"action":"accept_work","tunnel_id":tunnel,"reply_to":invite}),
        )
        .unwrap();
        let current = runtime
            .teams
            .mutate(&team.id, |team| {
                team.agents[1].status = "completed".into();
                team.tunnel_turns
                    .insert("b".into(), MAX_FOLLOWUPS_PER_AGENT);
                schedule(team);
                Ok(())
            })
            .unwrap();
        assert_eq!(current.tunnels[0].status, "open");
        assert!(current.tunnels[0].messages[0]
            .followup_closed_reason
            .is_some());
        assert!(current.tunnels[0].messages[1]
            .followup_closed_reason
            .is_none());
        let result = perform(runtime,&a,&json!({"action":"complete_work","tunnel_id":tunnel,"reply_to":invite,"text":"Checked 25 trial rows."})).unwrap();
        assert_eq!(result["tunnel"]["messages"][0]["kind"], "cowork_result");
    }

    #[test]
    fn asynchronous_delivery_resumes_once_and_unanswered_followup_does_not_deadlock_final() {
        let (fixture, team, _lease) = setup();
        let runtime = &fixture.runtime;
        let a = participant(&team, "a");
        perform(
            runtime,
            &a,
            &json!({"action":"ask","to":"b","topic":"question","text":"Which count is correct?"}),
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
            &json!({"action":"open","to":"b","topic":"new question","text":"Please answer","expects_reply":true}),
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
            let transcript = read_channel(runtime, &a, &opened);
            assert!(transcript["messages"][0].get("reply_to").is_none());
            assert!(transcript["messages"][0].get("expects_reply").is_none());
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
            assert!(!read_channel(runtime, &a, &closed)["close_reason"]
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
            read_channel(runtime, &b, &accepted)["messages"][1]["text"],
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
        let transcript = read_channel(runtime, &a, &semantic_open);
        assert!(transcript["messages"][0].get("reply_to").is_none());
        assert!(transcript["messages"][0].get("expects_reply").is_none());
        for action in ["send", "propose"] {
            assert!(perform(runtime,&a,&json!({"action":action,"tunnel_id":semantic_id,"text":"Bad reply","reply_to":"none"})).is_err());
        }
    }
}
