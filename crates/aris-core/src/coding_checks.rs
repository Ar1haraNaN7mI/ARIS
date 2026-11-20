//! Receipts for an explicitly commissioned local build or test. The verifier
//! never launches a process. Execution stays in the normal shell tool pipeline.
use crate::verification::VerificationCheck;
use crate::{Session, SessionEvent};
use std::collections::HashMap;

pub(crate) const PREFIX: &str = "command_passed:";

fn standard_check(command: &str) -> bool {
    // Only infer simple invocations. The entire text and its arguments must
    // still match the subsequently observed native shell call.
    if command.contains([
        '\n', '\r', ';', '&', '|', '<', '>', '$', '`', '#', '(', ')', '{', '}',
    ]) {
        return false;
    }
    let parts: Vec<_> = command.split_whitespace().collect();
    if let ["npm" | "npm.cmd" | "pnpm" | "pnpm.cmd" | "yarn" | "bun", "run", script, ..] =
        parts.as_slice()
    {
        let mut names = script.split(':');
        return matches!(
            names.next(),
            Some("test" | "build" | "typecheck" | "lint" | "check")
        ) && names.all(|name| {
            !name.is_empty()
                && name
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.'))
        });
    }
    matches!(
        parts.as_slice(),
        ["cargo", "test" | "check" | "build" | "clippy", ..]
            | ["go", "test" | "build" | "vet", ..]
            | [
                "npm" | "npm.cmd" | "pnpm" | "pnpm.cmd" | "yarn" | "bun",
                "test" | "build",
                ..
            ]
            | ["node", "--test", ..]
            | ["pytest" | "vitest", ..]
            | ["python" | "python3" | "py", "-m", "pytest" | "unittest", ..]
            | ["dotnet", "test" | "build", ..]
            | [
                "make" | "mvn" | "gradle" | "./gradlew",
                "test" | "check",
                ..
            ]
    )
}

fn run_instruction(before: &str) -> bool {
    let clause = before
        .rsplit(['。', '.', '!', '?', '！', '？', '\n', ';', '；'])
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase();
    let clause = clause.trim_end_matches([' ', ':', '：']);
    if matches!(
        clause,
        "验收命令" | "测试命令" | "acceptance command" | "test command"
    ) {
        return true;
    }
    let stem = ["execute", "rerun", "run", "运行", "执行", "重跑", "跑"]
        .iter()
        .find_map(|verb| clause.strip_suffix(verb));
    let Some(stem) = stem else { return false };
    let stem = stem.trim_end();
    if [
        "don't",
        "do not",
        "skip",
        "never",
        "not to",
        "不要",
        "不用",
        "无需",
        "别",
        "不必",
        "禁止",
        "如何",
        "怎么",
        "how to",
        "explain",
        "describe",
        "document",
        "example",
        "只解释",
        "只读",
        "示例",
        "举例",
    ]
    .iter()
    .any(|n| stem.contains(n))
    {
        return false;
    }
    matches!(stem, "" | "and" | "then")
        || stem.ends_with([',', '，', '、'])
        || [
            "please", " and", " then", "and then", "请", "并", "再", "后", "然后", "实际", "务必",
        ]
        .iter()
        .any(|s| stem.ends_with(s))
}

/// Free prose only opts in when it explicitly directs a marked or standalone
/// command. Other invocations can be specified with a declarative goal criterion.
pub(crate) fn add_requested(text: &str, criteria: &mut Vec<String>) {
    fn add(command: &str, criteria: &mut Vec<String>) {
        let command = command.trim();
        if standard_check(command) {
            let criterion = format!("{PREFIX}{command}");
            if !criteria.contains(&criterion) {
                criteria.push(criterion);
            }
        }
    }
    let mut previous = "";
    let mut fenced = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("```") || line.starts_with("~~~") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            if run_instruction(previous) {
                add(line, criteria);
            }
            previous = "";
            continue;
        }
        let mut rest = line;
        let mut continued = false;
        while let Some((before, args)) = rest.split_once('`') {
            let Some((command, after)) = args.split_once('`') else {
                break;
            };
            let requested = run_instruction(before)
                || (continued
                    && matches!(
                        before.trim(),
                        "and" | "then" | "," | "，" | "、" | "和" | "以及"
                    ));
            continued = requested && standard_check(command.trim());
            if requested {
                add(command, criteria);
            }
            rest = after;
        }
        if !line.contains('`') {
            if run_instruction(previous) {
                add(line, criteria);
            }
            // In prose only infer bare entry points. Mark arguments with
            // backticks or put the full command on its own line.
            let line = line.trim_end_matches(['.', '。']);
            for (index, _) in line.rmatch_indices(|c: char| c.is_whitespace()).take(3) {
                let (head, tail) = line.split_at(index);
                let parts: Vec<_> = tail.split_whitespace().collect();
                let bare = matches!(parts.as_slice(), ["pytest" | "vitest"])
                    || (parts.len() == 2 && !matches!(parts[0], "pytest" | "vitest"))
                    || matches!(parts.as_slice(), [_, "run" | "-m", _]);
                if bare && standard_check(tail.trim()) && run_instruction(head) {
                    add(tail, criteria);
                    break;
                }
            }
        }
        if !line.is_empty() {
            previous = line;
        }
    }
}

pub(crate) fn exit_code(text: &str) -> Option<i32> {
    text.trim_end()
        .lines()
        .last()?
        .strip_prefix("[exit ")?
        .strip_suffix(']')?
        .parse()
        .ok()
}

pub(crate) fn checks(criteria: &[String], session: &Session) -> Vec<VerificationCheck> {
    let mut checks: Vec<_> = criteria
        .iter()
        .filter(|c| c.trim().starts_with(PREFIX))
        .map(|c| VerificationCheck {
            criterion: c.trim().to_owned(),
            passed: false,
            detail: "No matching command finished in this turn.".into(),
        })
        .collect();
    if checks.is_empty() {
        return checks;
    }
    let Some(start) = session
        .events
        .iter()
        .rposition(|e| matches!(e, SessionEvent::TurnStart { .. }))
    else {
        return checks;
    };
    let mut calls = HashMap::new();
    for e in &session.events[start + 1..] {
        match e {
            SessionEvent::TurnEnd { .. } => break,
            SessionEvent::StepStart { .. } => calls.clear(),
            SessionEvent::ToolCall {
                call_id,
                name,
                arguments,
                ..
            } => {
                calls.insert(call_id, (name, arguments));
            }
            SessionEvent::ToolResult {
                call_id,
                name,
                ok,
                content,
                ..
            } => {
                let Some((called, args)) = calls.remove(call_id) else {
                    continue;
                };
                if called != name {
                    continue;
                }
                let command = args.get("command").and_then(|v| v.as_str()).map(str::trim);
                let code = if name == "shell" {
                    exit_code(content)
                } else {
                    None
                };
                let required = name == "shell"
                    && command.is_some_and(|c| {
                        checks.iter().any(|check| {
                            check.criterion[PREFIX.len()..].trim() == c && !c.is_empty()
                        })
                    });
                if required {
                    for check in &mut checks {
                        if Some(check.criterion[PREFIX.len()..].trim()) == command {
                            check.passed = *ok && code == Some(0);
                            check.detail = match code {
                                Some(status) => {
                                    format!("Native shell exit {status} (call {call_id}).")
                                }
                                None => {
                                    format!("No process exit receipt (call {call_id}): {content}")
                                }
                            };
                        }
                    }
                } else if (*ok
                    && matches!(
                        name.as_str(),
                        "edit_file" | "write_file" | "apply_patch" | "create_file"
                    ))
                    || (name == "shell" && code.is_some() && !command.is_some_and(safe_inspection))
                {
                    // Arbitrary shell commands can edit the tree even if they
                    // return nonzero. Rerun after such commands.
                    for check in checks.iter_mut().filter(|check| check.passed) {
                        check.passed = false;
                        check.detail=format!("The workspace may have changed after this check ({name}, call {call_id}); run it on the final tree.");
                    }
                }
            }
            _ => {}
        }
    }
    checks
}

fn safe_inspection(command: &str) -> bool {
    matches!(
        command,
        "git status"
            | "git status --short"
            | "git diff"
            | "git diff --stat"
            | "git diff --check"
            | "pwd"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use serde_json::json;
    fn start(s: &mut Session, id: &str) {
        s.append(SessionEvent::TurnStart {
            id: id.into(),
            profile_id: None,
            model_id: None,
            family: None,
            at: Utc::now(),
        });
    }
    fn tool(s: &mut Session, name: &str, args: serde_json::Value, success: bool, body: &str) {
        s.append(SessionEvent::ToolCall {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.into(),
            call_id: "reused".into(),
            arguments: args,
            at: Utc::now(),
        });
        s.append(SessionEvent::ToolResult {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.into(),
            call_id: "reused".into(),
            ok: success,
            content: body.into(),
            at: Utc::now(),
        });
    }

    #[test]
    fn concrete_run_requests_only_bind_the_complete_test_invocation() {
        let mut criteria = vec![];
        for prompt in [
            "Explain how to run `cargo test`.",
            "解释如何运行 `pytest -q`。",
            "Do not run `cargo test`.",
            "请不要运行 `cargo test`。",
            "Run `echo cargo test`",
            "Run `cargo test || echo passed`",
            "Run `cargo test # echo passed`",
            "只能解释怎么运行 `cargo test`。",
            "Explain the example, run `pytest`.",
            "Read the code and describe how to run `pnpm run test:unit`.",
            "请举例：\n```text\n运行 `cargo test`\n```",
            "Then run pytest someday",
            "Compare `cargo test` and `npm test`",
        ] {
            add_requested(prompt, &mut criteria);
        }
        assert!(criteria.is_empty(), "{criteria:?}");
        add_requested(
            "Fix the parser and run `cargo test parser`. Then run `npm test`.",
            &mut criteria,
        );
        add_requested(
            "修好后运行 `python -m unittest discover -s tests -v`。",
            &mut criteria,
        );
        add_requested("Run:\npytest -q", &mut criteria);
        add_requested("Fix it then run pnpm run test:unit.", &mut criteria);
        add_requested("验收命令：`pnpm.cmd run lint:types`。", &mut criteria);
        add_requested(
            "Then run `go vet ./...` and `go test ./...`.",
            &mut criteria,
        );
        assert_eq!(
            criteria,
            [
                "command_passed:cargo test parser",
                "command_passed:npm test",
                "command_passed:python -m unittest discover -s tests -v",
                "command_passed:pytest -q",
                "command_passed:pnpm run test:unit",
                "command_passed:pnpm.cmd run lint:types",
                "command_passed:go vet ./...",
                "command_passed:go test ./...",
            ]
        );
    }

    #[test]
    fn shell_proof_is_turn_scoped_and_invalidated_by_later_file_edits() {
        let c = vec![format!("{PREFIX}cargo test")];
        let mut s = Session::new();
        start(&mut s, "old-turn");
        tool(
            &mut s,
            "shell",
            json!({"command":"cargo test"}),
            true,
            "[exit 0]",
        );
        assert!(checks(&c, &s)[0].passed);
        start(&mut s, "new-turn");
        assert!(!checks(&c, &s)[0].passed);
        for (name, args) in [
            ("shell", json!({"command":"git commit -m tested"})),
            ("shell", json!({"command":"git diff --check"})),
            ("apply_patch", json!({"patch":"tests passed"})),
            ("shell", json!({"command":"cargo test # filtered"})),
        ] {
            tool(&mut s, name, args, true, "[exit 0]");
        }
        assert!(!checks(&c, &s)[0].passed);
        tool(
            &mut s,
            "shell",
            json!({"command":"echo cargo test"}),
            true,
            "[exit 0]",
        );
        tool(
            &mut s,
            "shell",
            json!({"command":"cargo test || true"}),
            true,
            "[exit 0]",
        );
        assert!(!checks(&c, &s)[0].passed);
        // Legacy outer tool ok=true still carries its real failing status.
        tool(
            &mut s,
            "shell",
            json!({"command":"cargo test"}),
            true,
            "spoofed [exit 0]\n[exit 2]",
        );
        assert!(checks(&c, &s)[0].detail.contains("exit 2"));
        assert!(!checks(&c, &s)[0].passed);
        tool(
            &mut s,
            "shell",
            json!({"command":"cargo test"}),
            true,
            "[exit 0]",
        );
        tool(
            &mut s,
            "shell",
            json!({"command":"git diff --check"}),
            true,
            "[exit 0]",
        );
        assert!(checks(&c, &s)[0].passed);
        tool(
            &mut s,
            "edit_file",
            json!({"path":"src/lib.rs"}),
            true,
            "replaced",
        );
        assert!(!checks(&c, &s)[0].passed);
        tool(
            &mut s,
            "shell",
            json!({"command":"cargo test"}),
            false,
            "Permission denied",
        );
        assert!(!checks(&c, &s)[0].passed);
        assert!(checks(&c, &s)[0].detail.contains("Permission denied"));
    }
}
