//! Model-facing JSON. The durable event/tunnel schema retains execution provenance.
use super::{optional_text, AgentTunnel, TunnelMessage};
use anyhow::{Context, Result};
use aris_llm::{ChatMessage, ContentPart, Role};
use serde::{ser::SerializeMap, Deserialize, Deserializer, Serializer};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

pub(super) struct Body {
    pub text: String,
    pub data: Option<Value>,
}

fn contains_wide_integer(value: &Value) -> bool {
    const MAX_SAFE: u64 = (1 << 53) - 1;
    match value {
        Value::Number(n) => {
            n.as_i64().is_some_and(|v| v.unsigned_abs() > MAX_SAFE)
                || n.as_u64().is_some_and(|v| v > MAX_SAFE)
        }
        Value::Array(values) => values.iter().any(contains_wide_integer),
        Value::Object(map) => map.values().any(contains_wide_integer),
        _ => false,
    }
}

// Browsers parse JSON numbers as doubles. Only payloads containing an integer
// outside their exact range need a separate rendering. Compute it from data
// on serialization, including when old archives are reopened.
pub(super) fn serialize_data<S: Serializer>(
    data: &Option<Value>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    let mut fields = serializer.serialize_map(None)?;
    if let Some(value) = data {
        fields.serialize_entry("data", value)?;
        if contains_wide_integer(value) {
            let display = serde_json::to_string_pretty(value).map_err(serde::ser::Error::custom)?;
            fields.serialize_entry("data_json", &display)?;
        }
    }
    fields.end()
}

pub(super) fn deserialize_data<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Value>, D::Error> {
    #[derive(Deserialize)]
    struct Payload {
        data: Option<Value>,
    }
    Ok(Payload::deserialize(deserializer)?.data)
}

pub(super) fn data(args: &Value) -> Option<&Value> {
    args.get("data").filter(|v| !v.is_null())
}

fn summary<'a>(kind: &str, data: &'a Value) -> &'a str {
    optional_text(data, "summary").unwrap_or(match kind {
        "question" => "请核对这组数据。",
        "answer" => "已回复所询事项。",
        "proposal" => "提出了协作方案。",
        "accept" => "已采纳方案。",
        "cowork_invite" => "邀请协作处理这项任务。",
        "cowork_accept" => "已接手协作任务。",
        "cowork_decline" => "已婉拒协作任务。",
        "cowork_result" => "已提交协作结果。",
        _ => "分享了协作数据。",
    })
}

pub(super) fn body(args: &Value, kind: &str, default: Option<&str>) -> Result<Body> {
    let data = data(args);
    let text = optional_text(args, "text")
        .or_else(|| data.map(|value| summary(kind, value)))
        .or(default)
        .context("Message requires text or data")?;
    Ok(Body {
        text: text.into(),
        data: data.cloned(),
    })
}

fn model_message(message: &TunnelMessage) -> Value {
    let mut row = json!({
        "id":message.id, "from":message.from, "to":message.to, "kind":message.kind
    });
    if let Some(data) = &message.data {
        row["data"] = data.clone();
        if message.text != summary(&message.kind, data) {
            row["text"] = json!(message.text);
        }
    } else {
        row["text"] = json!(message.text);
    }
    for (key, value) in [
        ("reply_to", &message.reply_to),
        ("proposal_id", &message.proposal_id),
        ("followup_closed_reason", &message.followup_closed_reason),
    ] {
        if let Some(value) = value {
            row[key] = json!(value);
        }
    }
    if message.expects_reply {
        row["expects_reply"] = Value::Bool(true);
    }
    if message.resume_on_reply {
        row["resume_on_reply"] = Value::Bool(true);
    }
    row
}

fn channel(tunnel: &AgentTunnel, messages: Vec<Value>) -> Value {
    let mut channel = json!({
        "id":tunnel.id, "participants":tunnel.participants, "topic":tunnel.topic,
        "status":tunnel.status, "messages":messages,
    });
    if let Some(decision) = &tunnel.decision {
        // The proposal carries its exact text/data. Include independent legacy decisions too.
        let mut accepted =
            json!({"proposal_id":decision.proposal_id,"accepted_by":decision.accepted_by});
        if !tunnel
            .messages
            .iter()
            .any(|m| m.id == decision.proposal_id && m.text == decision.text)
        {
            accepted["text"] = json!(decision.text);
        }
        channel["decision"] = accepted;
    }
    if let Some(reason) = &tunnel.close_reason {
        channel["close_reason"] = json!(reason);
    }
    channel
}

pub(crate) fn model_transcript(tunnel: &AgentTunnel) -> Value {
    channel(tunnel, tunnel.messages.iter().map(model_message).collect())
}

pub(super) fn receipt(tunnel: &AgentTunnel, appended: Option<usize>) -> Value {
    let messages: Vec<_> = appended
        .into_iter()
        .map(|i| {
            let m = &tunnel.messages[i];
            json!({"id":m.id,"kind":m.kind})
        })
        .collect();
    let mut compact = json!({"id":tunnel.id, "status":tunnel.status, "messages":messages});
    if let Some(decision) = &tunnel.decision {
        compact["decision"] = json!({"proposal_id":decision.proposal_id});
    }
    json!({"tunnel":compact, "blocking":false})
}

fn compact_legacy(packet: &mut Value, args: &str) -> bool {
    if packet["tunnel"].get("created_at").is_some() {
        let Ok(args) = serde_json::from_str::<Value>(args) else {
            return false;
        };
        let Ok(tunnel) = serde_json::from_value::<AgentTunnel>(packet["tunnel"].clone()) else {
            return false;
        };
        let appended = match args["action"].as_str() {
            Some("close") => None,
            Some("open") if optional_text(&args, "text").is_none() && data(&args).is_none() => None,
            _ => tunnel.messages.len().checked_sub(1),
        };
        *packet = receipt(&tunnel, appended);
        return true;
    }
    let mut changed = false;
    for tunnel in packet
        .get_mut("tunnels")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
    {
        if tunnel.get("created_at").is_some() {
            if let Ok(legacy) = serde_json::from_value::<AgentTunnel>(tunnel.clone()) {
                *tunnel = model_transcript(&legacy);
                changed = true;
            }
        }
    }
    changed
}

// Only an actual tool result or an inline runtime brief supplies transcript evidence.
// The file-backed form of a large brief has no messages to mark as seen.
pub(super) fn project_history(messages: &mut [ChatMessage]) -> HashSet<String> {
    let mut known = HashSet::new();
    let mut calls = HashMap::new();
    for message in messages {
        if message.role == Role::Assistant {
            calls.clear();
            for call in message.tool_calls.iter().flatten() {
                if let (Some(id), Some(f)) = (&call.id, &call.function) {
                    if f.name.as_deref() == Some("agent_tunnel") {
                        calls.insert(id.as_str(), f.arguments.as_deref().unwrap_or(""));
                    }
                }
            }
            continue;
        }
        let Some(ContentPart::Text(text)) = &message.content else {
            continue;
        };
        let packet: Value = match message.role {
            Role::Tool => {
                let Some(args) = message
                    .tool_call_id
                    .as_deref()
                    .and_then(|id| calls.remove(id))
                else {
                    continue;
                };
                let Ok(mut packet) = serde_json::from_str::<Value>(text) else {
                    continue;
                };
                if compact_legacy(&mut packet, args) {
                    message.content = Some(ContentPart::Text(packet.to_string()));
                }
                packet
            }
            Role::User if text.contains("aris.brief") => {
                let Ok(packet) = serde_json::from_str::<Value>(text.lines().last().unwrap_or(""))
                else {
                    continue;
                };
                if packet["type"] != "aris.brief" {
                    continue;
                }
                packet
            }
            _ => continue,
        };
        for tunnel in packet["tunnels"]
            .as_array()
            .into_iter()
            .flatten()
            .chain(packet.get("tunnel"))
        {
            for row in tunnel["messages"].as_array().into_iter().flatten() {
                if let Some(id) = row["id"].as_str() {
                    known.insert(id.to_owned());
                }
            }
        }
    }
    known
}

pub(super) fn inbox(
    tunnel: &AgentTunnel,
    recipient: &str,
    visible: &HashSet<String>,
) -> Option<Value> {
    // Include the original request/proposal when the recipient can see a reply
    // but its outgoing tool call has already left the bounded model history.
    let mut needed = HashSet::new();
    for m in tunnel.messages.iter().rev() {
        if (m.to == recipient || needed.contains(m.id.as_str())) && !visible.contains(&m.id) {
            needed.insert(m.id.as_str());
            needed.extend(m.reply_to.as_deref());
            needed.extend(m.proposal_id.as_deref());
        }
    }
    let rows: Vec<_> = tunnel
        .messages
        .iter()
        .filter(|m| needed.contains(m.id.as_str()) && !visible.contains(&m.id))
        .map(model_message)
        .collect();
    (!rows.is_empty()).then(|| channel(tunnel, rows))
}
