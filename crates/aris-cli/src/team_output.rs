use anyhow::Result;
use aris_core::{
    harness::{Team, TeamAgent},
    is_standalone_json_reply,
    synthesis::{DeliveryKind, DeliverySource},
};
use clap::ValueEnum;
use std::{collections::HashSet, path::Path};

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum Format {
    Text,
    Json,
}

pub(crate) fn text_requested(format: Option<Format>, terminal: bool) -> bool {
    format.map_or(terminal, |format| format == Format::Text)
}

/// Only turns finished since this invocation can contribute a public answer.
pub(crate) fn render(
    team: &Team,
    round_start: usize,
    admin_request: Option<&str>,
    mut delivery: impl FnMut(&TeamAgent) -> Result<Option<DeliverySource>>,
) -> Result<String> {
    let events = team.events.get(round_start..).unwrap_or_default();
    let admin = team
        .admin
        .as_ref()
        .filter(|admin| Some(admin.request_id.as_str()) == admin_request);
    let direct_answer = admin.filter(|admin| {
        admin.phase == "answered" && events.iter().any(|e| e.kind == "admin_answered")
    });
    if let Some(admin) = direct_answer
        .filter(|a| !a.response.trim().is_empty() && !is_standalone_json_reply(&a.response))
    {
        return Ok(admin.response.clone());
    }
    let mut sections = Vec::new();
    let ended: HashSet<_> = events
        .iter()
        .filter(|e| matches!(e.kind.as_str(), "agent_finished" | "agent_error"))
        .filter_map(|e| e.agent_id.as_deref())
        .collect();
    if matches!(team.status.as_str(), "failed" | "cancelled") {
        sections.push(
            if team.status == "cancelled" {
                "任务已取消。"
            } else {
                "任务未完成。"
            }
            .into(),
        );
        if let Some(reason) = admin.and_then(|admin| admin.error.as_deref()) {
            sections.push(reason.to_owned());
        }
        for role in team
            .agents
            .iter()
            .filter(|r| ended.contains(r.id.as_str()) && r.status == "failed")
        {
            if let Some(error) = &role.error {
                sections.push(format!("{} · {error}", role.name));
            }
        }
    }
    let finished: Vec<_> = team
        .agents
        .iter()
        .filter(|role| ended.contains(role.id.as_str()) && role.status == "completed")
        .collect();
    let mut publications: Vec<_> = finished
        .iter()
        .copied()
        .filter(|role| {
            matches!(
                role.delivery,
                Some(DeliveryKind::Final | DeliveryKind::Independent)
            ) || !finished
                .iter()
                .any(|other| other.depends_on.contains(&role.id))
        })
        .collect();
    publications.sort_by_key(|role| role.delivery != Some(DeliveryKind::Final));
    let label_roles = publications.len() > 1;
    for role in publications {
        let Some(source) = delivery(role)? else {
            sections.push(format!("{} 的本轮交付缺少可核对的快照。", role.name));
            continue;
        };
        let mut contents = Vec::new();
        for artifact in &source.artifacts {
            let extension = Path::new(&artifact.path)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            if matches!(extension.as_str(), "md" | "markdown" | "txt" | "rst")
                && !artifact.text.trim().is_empty()
                && !is_standalone_json_reply(&artifact.text)
            {
                contents.push(format!("{}\n\n{}", artifact.path, artifact.text));
            } else {
                contents.push(format!("已生成 {}", artifact.path));
            }
        }
        let structured_answer = is_standalone_json_reply(&source.text);
        if !source.text.trim().is_empty()
            && !structured_answer
            && !source.artifacts.iter().any(|a| a.text == source.text)
        {
            contents.insert(0, source.text);
        }
        if contents.is_empty() {
            let message = if structured_answer {
                "本轮已完成，结果以结构化数据保存。"
            } else {
                "本轮已完成，没有文字交付。"
            };
            contents.push(message.to_owned());
        }
        if label_roles {
            contents.insert(0, role.name.clone());
        }
        sections.push(contents.join("\n\n"));
    }
    if sections.is_empty() {
        sections.push(
            if direct_answer.is_some_and(|a| is_standalone_json_reply(&a.response)) {
                "本轮管理员回答已保存为结构化数据。".into()
            } else {
                "本轮没有新增交付。".into()
            },
        );
    }
    Ok(sections.join("\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn role(id: &str, kind: Option<&str>, depends_on: &[&str]) -> Value {
        json!({"id":id,"name":id,"profile_id":"kei","delivery":kind,"depends_on":depends_on,"status":"completed","output":"old stream"})
    }
    fn team(roles: Vec<Value>, events: &[&str]) -> Team {
        serde_json::from_value(json!({"id":"test","name":"test","goal":"test","status":"completed",
            "agents":roles,"files":[],"handoffs":[],"messages":[],"checkpoints":[],"created_at":"now",
            "events":events.iter().map(|id|json!({"at":"now","agent_id":id,"kind":"agent_finished","message":"finished"})).collect::<Vec<_>>()})).unwrap()
    }
    fn answer(agent: &TeamAgent) -> Result<Option<DeliverySource>> {
        Ok(Some(serde_json::from_value(json!({
            "agent_id":agent.id,"session_id":"s","run_id":"r","task_id":"k","turn_id":"t","event_id":"e",
            "text":format!("已核对 {}",agent.name),"artifacts":[],"references":[]
        }))?))
    }

    #[test]
    fn terminal_text_and_pipe_json_can_be_overridden() {
        assert!(text_requested(None, true));
        assert!(!text_requested(None, false));
        assert!(text_requested(Some(Format::Text), false));
        assert!(!text_requested(Some(Format::Json), true));
    }

    #[test]
    fn current_final_and_independent_replies_survive_a_mixed_dispatch() {
        let t = team(
            vec![
                role("old", None, &[]),
                role("evidence", Some("supporting"), &[]),
                role("独立回复", Some("independent"), &[]),
                role("最终回复", Some("final"), &["evidence", "独立回复"]),
            ],
            &["old", "evidence", "独立回复", "最终回复"],
        );
        let output = render(&t, 1, None, answer).unwrap();
        assert_eq!(
            output,
            "最终回复\n\n已核对 最终回复\n\n独立回复\n\n已核对 独立回复"
        );
        assert_eq!(render(&t, 4, None, answer).unwrap(), "本轮没有新增交付。");
    }

    #[test]
    fn only_the_current_admin_answer_can_win_over_workers_or_failure() {
        let mut t = team(vec![role("研究", None, &[])], &["研究"]);
        t.admin = Some(serde_json::from_value(json!({"request_id":"new","session_id":"admin","profile_id":"kei","model_id":"A","family":"gpt","phase":"answered","response":"今天交付两组数据。"})).unwrap());
        t.events.push(
            serde_json::from_value(
                json!({"at":"now","kind":"admin_answered","message":"answered"}),
            )
            .unwrap(),
        );
        assert_eq!(
            render(&t, 1, Some("new"), answer).unwrap(),
            "今天交付两组数据。"
        );
        assert_eq!(render(&t, 1, None, answer).unwrap(), "本轮没有新增交付。");
        t.status = "failed".into();
        t.agents[0].status = "failed".into();
        t.agents[0].error = Some("缺少输入文件".into());
        assert_eq!(
            render(&t, 0, Some("other"), answer).unwrap(),
            "任务未完成。\n\n研究 · 缺少输入文件"
        );
    }

    #[test]
    fn legacy_structured_answers_and_text_artifacts_publish_only_readable_content() {
        let mut t = team(vec![role("编辑", Some("final"), &[])], &["编辑"]);
        t.admin = Some(serde_json::from_value(json!({"request_id":"new","session_id":"admin","profile_id":"kei","model_id":"A","family":"gpt","phase":"answered","response":"{\"action\":\"a\"}"})).unwrap());
        t.events.push(
            serde_json::from_value(
                json!({"at":"now","kind":"admin_answered","message":"answered"}),
            )
            .unwrap(),
        );
        let output = render(&t, 0, Some("new"), |agent| {
            let mut source = answer(agent)?.unwrap();
            source.text = "[{\"action\":\"b\"}]".into();
            source.artifacts = vec![
                ("数据.txt", "[1,2]"),
                ("表格.md", "```json\n{\"action\":\"c\"}\n```"),
                ("结论.md", "两种方案均已覆盖。\n\n```json\n[1,2]\n```"),
            ].into_iter().map(|(path, text)| serde_json::from_value(json!({"path":path,"text":text,"sha256":"s","size_bytes":text.len(),"event_id":"e"})).unwrap()).collect();
            Ok(Some(source))
        }).unwrap();
        assert_eq!(output, "已生成 数据.txt\n\n已生成 表格.md\n\n结论.md\n\n两种方案均已覆盖。\n\n```json\n[1,2]\n```");
        assert_eq!(
            render(&t, 1, Some("new"), answer).unwrap(),
            "本轮管理员回答已保存为结构化数据。"
        );
    }
}
