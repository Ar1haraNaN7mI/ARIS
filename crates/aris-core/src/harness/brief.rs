//! A worker's model-facing assignment. Durable team records keep their full audit metadata.
use super::{Team, TeamAgent};
use serde_json::{json, Value};

pub(crate) const WORKER_GUIDANCE: &str = "Use this brief. Later instructions take precedence. A text_ref names a brief field; summary_from names the source delivery. Use your own browser session and edit your owned paths. Coordinate via agent_tunnel. Return findings, sources and delivered paths in the user's language; honor requested artifact formats.";

pub(crate) fn assignment(team: &Team, agent: &TeamAgent) -> Value {
    let mut packet = json!({
        "type": "aris.brief", "agent_id": agent.id, "name": agent.name,
        "role": agent.role, "goal": team.goal,
    });
    let assignment_key = if agent.prompt == team.goal {
        packet["assignment_ref"] = json!("goal");
        "goal"
    } else {
        packet["assignment"] = json!(agent.prompt);
        "assignment"
    };
    if !agent.files.is_empty() {
        packet["owned_output_files"] = json!(agent.files);
    }
    let shared: Vec<_> = team
        .files
        .iter()
        .filter(|path| !agent.files.contains(path))
        .collect();
    if !shared.is_empty() {
        packet["other_shared_files"] = json!(shared);
    }
    let peers: Vec<_> = team
        .agents
        .iter()
        .filter(|peer| {
            peer.id != agent.id
                && team
                    .admin
                    .as_ref()
                    .is_none_or(|admin| peer.session_id != admin.session_id)
                && (agent.delivery.is_some() || peer.subagent_id.is_some())
        })
        .map(|peer| json!({"agent_id":peer.id, "name":peer.name, "role":peer.role}))
        .collect();
    if !peers.is_empty() {
        packet["available_peers"] = Value::Array(peers);
    }
    let instructions: Vec<_> = team
        .messages
        .iter()
        .filter(|m| !m.queued && m.agent_id.as_deref().is_none_or(|id| id == agent.id))
        .map(|m| {
            let source = m.source.as_deref().unwrap_or(if team.admin.is_some() {
                "unknown"
            } else {
                "user"
            });
            let mut row = json!({"id": m.id, "source": source, "at": m.at});
            if let Some(target) = &m.agent_id {
                row["to"] = json!(target);
            }
            if m.text == agent.prompt {
                row["text_ref"] = json!(assignment_key);
            } else if m.text == team.goal {
                row["text_ref"] = json!("goal");
            } else {
                row["text"] = json!(m.text);
            }
            row
        })
        .collect();
    if !instructions.is_empty() {
        packet["instructions"] = Value::Array(instructions);
    }
    packet
}

/// A handoff often repeats the sender's entire published answer. Reference it
/// only when this exact content is also available in the recipient's packet.
pub(crate) fn add_handoffs(
    packet: &mut Value,
    team: &Team,
    recipient: &str,
    has_source_text: impl Fn(&str, &str) -> bool,
) {
    let rows: Vec<_> = team
        .handoffs
        .iter()
        .filter(|h| h.to == recipient)
        .map(|h| {
            let mut row =
                json!({"id": h.id, "from": h.from, "at": h.at, "instruction": h.instruction});
            if !h.summary.is_empty() {
                if has_source_text(&h.from, &h.summary) {
                    row["summary_from"] = json!(h.from);
                } else {
                    row["summary"] = json!(h.summary);
                }
            }
            row
        })
        .collect();
    if !rows.is_empty() {
        packet["handoffs"] = Value::Array(rows);
    }
}

pub(super) fn worker_prompt(team: &Team, agent: &TeamAgent) -> String {
    let mut packet = assignment(team, agent);
    let dependencies: Vec<_> = team
        .agents
        .iter()
        .filter(|source| agent.depends_on.contains(&source.id))
        .collect();
    add_handoffs(&mut packet, team, &agent.id, |id, text| {
        dependencies
            .iter()
            .any(|src| src.id == id && src.output == text)
    });
    if !dependencies.is_empty() {
        packet["source_deliveries"] = Value::Array(dependencies.iter().map(|source| {
            json!({"agent_id": source.id, "name": source.name, "profile_id": source.profile_id, "text": source.output})
        }).collect());
    }
    format!("{WORKER_GUIDANCE}\n{packet}")
}

pub(super) fn cowork_prompt(agent: &TeamAgent, invitations: &[String]) -> String {
    let mut packet = json!({"type":"aris.brief","agent_id":agent.id,"invitations":invitations});
    if !agent.files.is_empty() {
        packet["owned_output_files"] = json!(agent.files);
    }
    format!("Handle only these co-work invitations in your lane. Read the peer transcript, accept and perform each contribution, then complete_work or decline_work with its invitation ID. Ask the requester for missing inputs with resume_on_reply when needed. Your primary assignment starts separately.\n{packet}")
}
