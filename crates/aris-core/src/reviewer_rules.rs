//! Cross-task learning for native file-edit recovery. Only typed outcomes leave
//! the source session; file contents, paths and error prose cannot become a rule.
use crate::{settings::atomic_write, Runtime, SessionEvent};
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, PartialOrd, Ord, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EditFailure {
    MissingAnchor,
    AmbiguousAnchor,
}

impl EditFailure {
    pub(crate) fn recognize(tool: &str, ok: bool, body: &str) -> Option<Self> {
        if ok || tool != "edit_file" {
            return None;
        }
        if body.starts_with("old_string not found in ") {
            return Some(Self::MissingAnchor);
        }
        let (count, action) = body
            .strip_prefix("old_string found ")?
            .split_once(" times; ")?;
        if count.parse::<usize>().ok()? > 1 && action == "set replace_all=true or make it unique" {
            Some(Self::AmbiguousAnchor)
        } else {
            None
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::MissingAnchor => "edit_file.missing_anchor.read_file",
            Self::AmbiguousAnchor => "edit_file.ambiguous_anchor.read_file",
        }
    }
    fn advice(self) -> &'static str {
        match self{
        Self::MissingAnchor=>"For the edit_file mismatch just observed, use read_file on that same file, take the exact current old_string from the fresh result, and retry the intended edit.",
        Self::AmbiguousAnchor=>"For the ambiguous edit_file match just observed, read the same file with read_file. Select a unique span using the surrounding lines and retry the intended edit.",
    }
    }
    fn description(self) -> &'static str {
        match self {
            Self::MissingAnchor => "原替换文本在文件中未匹配时，先重读同一文件，取当前原文再编辑。",
            Self::AmbiguousAnchor => {
                "原替换文本有多个匹配时，先重读同一文件，用上下文确定唯一替换范围。"
            }
        }
    }
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Route {
    Direct,
    ReadFirst,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryObservation {
    id: String,
    class: EditFailure,
    route: Route,
    success: bool,
}

struct Pending<'a> {
    id: &'a str,
    class: EditFailure,
    failed_step: &'a str,
    read_step: Option<&'a str>,
    replacement: &'a str,
}

fn target(args: &Value) -> Option<PathBuf> {
    let p = Path::new(args.get("path")?.as_str()?);
    if p.as_os_str().is_empty() {
        return None;
    }
    // Keep case and parent components; two different files must not count as a recovery.
    Some(p.components().collect())
}

pub fn observations(
    events: &[SessionEvent],
    turn: Option<&str>,
    window: Option<(DateTime<Utc>, DateTime<Utc>)>,
) -> Vec<RecoveryObservation> {
    let mut out = Vec::new();
    let mut current_turn = None;
    let mut step = None;
    let mut calls = HashMap::new();
    let mut mismatches = HashMap::<PathBuf, Pending>::new();
    for e in events {
        match e {
            SessionEvent::TurnStart { id, .. } => {
                current_turn = Some(id.as_str());
                step = None;
                calls.clear();
                mismatches.clear();
            }
            SessionEvent::TurnEnd { .. } => {
                current_turn = None;
                step = None;
                calls.clear();
                mismatches.clear();
            }
            SessionEvent::StepStart { id, .. } => step = Some(id.as_str()),
            _ => {}
        }
        if current_turn.is_none()
            || turn.is_some_and(|wanted| Some(wanted) != current_turn)
            || window.is_some_and(|(a, b)| e.occurred_at() < a || e.occurred_at() > b)
        {
            continue;
        }
        let Some(step) = step else { continue };
        match e {
            SessionEvent::ToolCall {
                call_id,
                name,
                arguments,
                ..
            } => {
                // A non-observational intervening call can change the target by
                // another route. Do not credit this recovery with its outcome.
                match name.as_str() {
                    "read_file" | "edit_file" => {
                        if let Some(path) = target(arguments) {
                            calls.insert(call_id.as_str(), (name.as_str(), path, step, arguments));
                        }
                    }
                    _ => {
                        calls.clear();
                        mismatches.clear();
                    }
                }
            }
            SessionEvent::ToolResult {
                id,
                call_id,
                name,
                ok,
                content,
                ..
            } => {
                let Some((called, path, at_step, args)) = calls.remove(call_id.as_str()) else {
                    continue;
                };
                if called != name || at_step != step {
                    continue;
                }
                if called == "read_file" {
                    if let Some(p) = mismatches.get_mut(&path) {
                        if *ok && p.failed_step != step {
                            p.read_step = Some(step);
                        } else {
                            mismatches.remove(&path);
                        }
                    }
                    continue;
                }
                let Some(replacement) = args.get("new_string").and_then(Value::as_str) else {
                    mismatches.remove(&path);
                    continue;
                };
                if args.get("old_string").and_then(Value::as_str) == Some(replacement)
                    || args.get("replace_all").and_then(Value::as_bool) == Some(true)
                {
                    mismatches.remove(&path);
                    continue;
                }
                let next_failure = EditFailure::recognize(name, *ok, content);
                if let Some(p) = mismatches.remove(&path) {
                    // A model can only revise an edit from a result on a later
                    // model step. Sibling calls in one response are not evidence.
                    if p.replacement == replacement
                        && p.failed_step != step
                        && p.read_step != Some(step)
                        && (*ok || next_failure == Some(p.class))
                    {
                        out.push(RecoveryObservation {
                            id: p.id.to_string(),
                            class: p.class,
                            route: if p.read_step.is_some() {
                                Route::ReadFirst
                            } else {
                                Route::Direct
                            },
                            success: *ok,
                        });
                    }
                }
                if let Some(class) = next_failure {
                    mismatches.insert(
                        path,
                        Pending {
                            id,
                            class,
                            failed_step: step,
                            read_step: None,
                            replacement,
                        },
                    );
                }
            }
            _ => {}
        }
    }
    out
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleEvaluation {
    pub rule: String,
    pub status: String,
    pub description: String,
    pub baseline: String,
    pub candidate: String,
    pub version: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Example {
    group: String,
    route: Route,
    success: bool,
}
#[derive(Default, Serialize, Deserialize)]
struct Rule {
    active: bool,
    version: u64,
    epoch: usize,
    examples: Vec<Example>,
}
#[derive(Default, Serialize, Deserialize)]
struct State {
    rules: BTreeMap<EditFailure, Rule>,
    events: HashSet<String>,
    receipts: BTreeMap<String, Vec<RuleEvaluation>>,
}
fn read(dir: &Path) -> Result<State> {
    match File::open(dir.join("state.json")) {
        Ok(f) => Ok(serde_json::from_reader(f)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
        Err(e) => Err(e.into()),
    }
}
fn open_lock(dir: &Path) -> Result<File> {
    fs::create_dir_all(dir)?;
    Ok(OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join("lock"))?)
}

pub fn clear(runtime: &Runtime) -> Result<()> {
    let dir = runtime.outer_home.join("learn/edit-recovery-v1");
    let lock = open_lock(&dir)?;
    fs2::FileExt::lock_exclusive(&lock)?;
    let path = dir.join("state.json");
    if path.try_exists()? {
        fs::remove_file(path)?;
    }
    Ok(())
}
impl Rule {
    fn update(&mut self, class: EditFailure, new_counterexample: bool) -> RuleEvaluation {
        // Vote once per workflow and route. Mixed outcomes within a workflow
        // count as a failure rather than letting a repeated loop dominate rates.
        let mut direct = HashMap::<&str, bool>::new();
        let mut read = HashMap::<&str, bool>::new();
        for x in &self.examples[self.epoch..] {
            let groups = if x.route == Route::ReadFirst {
                &mut read
            } else {
                &mut direct
            };
            *groups.entry(&x.group).or_insert(true) &= x.success;
        }
        let good = read.values().filter(|x| **x).count();
        let baseline = direct.values().filter(|x| **x).count();
        let independent_control = direct.iter().any(|(g, worked)| {
            !worked
                && read
                    .iter()
                    .filter(|(id, passed)| *id != g && **passed)
                    .count()
                    >= 2
        });
        let improved =
            (good as u128) * (direct.len() as u128) > (baseline as u128) * (read.len() as u128);
        let mut status = if self.active { "retained" } else { "proposed" };
        if self.active && (new_counterexample || !improved) {
            self.active = false;
            self.version += 1;
            status = "retired";
        } else if !self.active && good >= 2 && independent_control && improved {
            self.active = true;
            self.version += 1;
            status = "applied";
        }
        let result = RuleEvaluation {
            rule: class.name().into(),
            status: status.into(),
            description: class.description().into(),
            baseline: format!(
                "同一目标直接重试，成功 {baseline}/{} 个独立任务",
                direct.len()
            ),
            candidate: format!("重读同一目标后重试，成功 {good}/{} 个独立任务", read.len()),
            version: self.version,
        };
        if status == "retired" {
            // A fresh cohort must re-qualify this rule after contradictory evidence.
            self.epoch = self.examples.len();
        }
        result
    }
}

pub fn assess(
    runtime: &Runtime,
    review_id: &str,
    group_id: &str,
    facts: &BTreeMap<String, Value>,
) -> Result<Vec<RuleEvaluation>> {
    if !runtime.learn.generation_enabled() || !runtime.settings.read().memory_inject {
        return Ok(vec![]);
    }
    let mut observations = vec![];
    let mut collect = |trace: &Value| -> Result<()> {
        if let Some(rows) = trace.get("recovery_observations").and_then(Value::as_array) {
            for row in rows {
                observations.push(serde_json::from_value::<RecoveryObservation>(row.clone())?);
            }
        }
        Ok(())
    };
    for (key, fact) in facts {
        if key == "turn" {
            collect(fact)?;
        }
        if key.starts_with("lane/") {
            if let Some(t) = fact.get("trace") {
                collect(t)?;
            }
        }
        if key == "attempts" {
            if let Some(runs) = fact.as_array() {
                for trace in runs.iter().filter_map(|r| r.get("trace")) {
                    collect(trace)?;
                }
            }
        }
    }
    if observations.is_empty() {
        return Ok(vec![]);
    }
    let dir = runtime.outer_home.join("learn/edit-recovery-v1");
    let lock = open_lock(&dir)?;
    fs2::FileExt::lock_exclusive(&lock)?;
    let mut state = read(&dir)?;
    if let Some(existing) = state.receipts.get(review_id) {
        return Ok(existing.clone());
    }
    let known_groups = state
        .rules
        .iter()
        .filter_map(|(&class, r)| {
            r.examples[r.epoch..]
                .iter()
                .any(|e| e.group == group_id)
                .then_some(class)
        })
        .collect::<HashSet<_>>();
    let mut changed = BTreeMap::<EditFailure, bool>::new();
    for x in observations {
        if !state.events.insert(x.id) {
            continue;
        }
        let r = state.rules.entry(x.class).or_default();
        let counterexample = r.active
            && x.route == Route::ReadFirst
            && !x.success
            && !known_groups.contains(&x.class);
        *changed.entry(x.class).or_default() |= counterexample;
        r.examples.push(Example {
            group: group_id.into(),
            route: x.route,
            success: x.success,
        });
    }
    if changed.is_empty() {
        return Ok(vec![]);
    }
    let results: Vec<_> = changed
        .into_iter()
        .map(|(class, counter)| state.rules.get_mut(&class).unwrap().update(class, counter))
        .collect();
    state.receipts.insert(review_id.into(), results.clone());
    atomic_write(&dir.join("state.json"), &serde_json::to_vec(&state)?)?;
    Ok(results)
}

/// Attach only the predefined instruction for an empirically active rule,
/// to the next request after an actual native tool failure.
pub(crate) fn apply(
    runtime: &Runtime,
    class: EditFailure,
    messages: &mut [aris_llm::ChatMessage],
) -> Result<bool> {
    if !runtime.config.learn.enabled || !runtime.settings.read().memory_inject {
        return Ok(false);
    }
    let dir = runtime.outer_home.join("learn/edit-recovery-v1");
    if !dir.try_exists()? {
        return Ok(false);
    }
    let lock = open_lock(&dir)?;
    fs2::FileExt::lock_shared(&lock)?;
    if !read(&dir)?.rules.get(&class).is_some_and(|r| r.active) {
        return Ok(false);
    }
    for m in messages {
        if m.role == aris_llm::Role::System {
            if let Some(aris_llm::ContentPart::Text(body)) = &mut m.content {
                body.push('\n');
                body.push_str(class.advice());
                return Ok(true);
            }
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::RuntimeFixture;
    use serde_json::json;
    const PATH: &str = "sensitive/market-notes.md";
    const MISSING: &str = "old_string not found in sensitive/market-notes.md";
    const AMBIGUOUS: &str = "old_string found 2 times; set replace_all=true or make it unique";

    // Build the same turn -> step -> call/result boundaries as the ordinary agent loop.
    fn trace(id: &str, calls: &[(usize, &str, &str, &str, bool, &str)]) -> Vec<SessionEvent> {
        let mut result = vec![];
        let at = |n: i64| DateTime::from_timestamp_millis(1_000 + n).unwrap();
        result.push(SessionEvent::TurnStart {
            id: id.into(),
            at: at(0),
            profile_id: None,
            model_id: None,
            family: None,
        });
        let mut previous = 0;
        for (i, &(step, name, path, replacement, ok, content)) in calls.iter().enumerate() {
            let call_id = format!("{id}-call-{i}");
            let t = 3 * (i as i64) + 1;
            if step != previous {
                result.push(SessionEvent::StepStart {
                    id: format!("{id}-step-{step}"),
                    turn_id: id.into(),
                    at: at(t),
                });
                previous = step;
            }
            let arguments = if name == "edit_file" {
                json!({"path":path,"old_string":"stale","new_string":replacement})
            } else {
                json!({"path":path})
            };
            result.push(SessionEvent::ToolCall {
                id: format!("{id}-request-{i}"),
                call_id: call_id.clone(),
                name: name.into(),
                arguments,
                at: at(t + 1),
            });
            result.push(SessionEvent::ToolResult {
                id: format!("{id}-result-{i}"),
                call_id,
                name: name.into(),
                ok,
                content: content.into(),
                at: at(t + 2),
            });
        }
        result.push(SessionEvent::TurnEnd {
            id: id.into(),
            at: at(3 * calls.len() as i64 + 1),
        });
        result
    }

    fn rows(id: &str, read: bool, success: bool) -> Vec<RecoveryObservation> {
        let mut ops = vec![(1, "edit_file", PATH, "published text", false, MISSING)];
        if read {
            ops.push((2, "read_file", PATH, "", true, "user secret"));
        }
        ops.push((
            3,
            "edit_file",
            PATH,
            "published text",
            success,
            if success { "updated" } else { MISSING },
        ));
        observations(&trace(id, &ops), None, None)
    }

    #[test]
    fn only_attributes_a_matched_edit_to_a_subsequent_model_step_in_the_same_turn_and_file() {
        let ops = [
            (1, "edit_file", PATH, "new", false, AMBIGUOUS),
            (2, "read_file", PATH, "", true, "private body"),
            (3, "edit_file", PATH, "new", true, "updated"),
        ];
        let e = trace("t", &ops);
        let obs = observations(&e, Some("t"), None);
        assert_eq!(obs.len(), 1);
        assert_eq!(obs[0].route, Route::ReadFirst);
        assert_eq!(obs[0].class, EditFailure::AmbiguousAnchor);
        assert!(obs[0].success);
        let stored = serde_json::to_string(&obs).unwrap();
        for sensitive in [PATH, "new", "private body"] {
            assert!(!stored.contains(sensitive));
        }
        assert!(observations(&e, Some("different"), None).is_empty());
        assert!(observations(
            &e,
            None,
            Some((e[4].occurred_at(), e.last().unwrap().occurred_at()))
        )
        .is_empty());
        for (name, step, path, ok) in [
            ("shell", 2, PATH, true),
            ("read_file", 3, PATH, true),
            ("read_file", 2, PATH, false),
        ] {
            let mut edited = ops;
            edited[1] = (step, name, path, "", ok, "");
            assert!(observations(&trace("x", &edited), None, None).is_empty());
        }
        let mut changed = ops;
        changed[2].3 = "a separate change";
        assert!(observations(&trace("x", &changed), None, None).is_empty());
        let mut changed = ops;
        changed[2].5 = "approval denied";
        changed[2].4 = false;
        assert!(observations(&trace("x", &changed), None, None).is_empty());
        let mut changed = ops;
        changed[1].2 = "different.md";
        assert_eq!(
            observations(&trace("x", &changed), None, None)[0].route,
            Route::Direct
        );
    }

    #[test]
    fn compares_distinct_task_outcomes_activates_in_a_later_request_and_retires_on_counterevidence()
    {
        let f = RuntimeFixture::new(crate::SessionSettings::default(), true);
        let evidence = |key: &str, id: &str, read, success| {
            let trace = json!({"recovery_observations":rows(id,read,success)});
            BTreeMap::from([(
                key.into(),
                match key {
                    "turn" => trace,
                    "attempts" => json!([{"trace":trace}]),
                    _ => json!({"trace":trace}),
                },
            )])
        };
        let learn = |key: &str, review: &str, group: &str, read, worked| {
            assess(
                &f.runtime,
                review,
                group,
                &evidence(key, review, read, worked),
            )
            .unwrap()
            .remove(0)
        };
        let uses_hint = || {
            let mut msg = [
                aris_llm::ChatMessage::system("base"),
                aris_llm::ChatMessage::user("question"),
            ];
            let used = apply(&f.runtime, EditFailure::MissingAnchor, &mut msg).unwrap();
            assert_eq!(msg[0].text().contains("read_file"), used);
            assert_eq!(msg[1].text(), "question");
            used
        };
        assert_eq!(learn("turn", "1", "chat-a", true, true).status, "proposed");
        assert_eq!(
            learn("attempts", "2", "chat-a", true, true).status,
            "proposed"
        );
        assert_eq!(
            learn("lane/b", "3", "team-b", true, true).status,
            "proposed"
        );
        assert!(!uses_hint());
        let active = learn("attempts", "4", "task-c", false, false);
        assert_eq!((active.status.as_str(), active.version), ("applied", 1));
        assert!(active.candidate.contains("2/2"));
        assert!(uses_hint());
        f.runtime.settings.write().memory_inject = false;
        assert!(!uses_hint());
        f.runtime.settings.write().memory_inject = true;
        assert!(assess(
            &f.runtime,
            "duplicate-evidence",
            "other",
            &evidence("turn", "4", false, false)
        )
        .unwrap()
        .is_empty());
        assert_eq!(learn("turn", "5", "chat-d", true, false).status, "retired");
        assert!(!uses_hint());
        assert_eq!(learn("attempts", "4", "task-c", false, false).version, 1);
        assert!(!uses_hint());
        assert_eq!(learn("turn", "6", "chat-e", true, true).status, "proposed");
        clear(&f.runtime).unwrap();
        let reset = learn("attempts", "4", "task-c", false, false);
        assert_eq!((reset.status.as_str(), reset.version), ("proposed", 0));
    }
}
