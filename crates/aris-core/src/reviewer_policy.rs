//! Versioned scheduling rules evaluated against native lane executions.
//! A replay compares scheduling order, holding observed service times, file
//! reservations, dependencies and the slot budget constant.
use crate::harness::{admin_workers, selected_for_run, Team, TeamAgent};
use crate::{settings::atomic_write, Runtime};
use anyhow::{bail, ensure, Context, Result};
use chrono::DateTime;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::path::Path;

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum SchedulingPolicy {
    #[default]
    RosterOrder,
    LongestDependencyChain,
    MostDirectDependents,
    Weighted {
        chain: u8,
        fanout: u8,
    },
}

impl SchedulingPolicy {
    pub fn display_name(self) -> String {
        match self {
            Self::RosterOrder => "派工顺序优先".into(),
            Self::LongestDependencyChain => "下游依赖链最长的泳道优先".into(),
            Self::MostDirectDependents => "直接下游最多的泳道优先".into(),
            Self::Weighted { chain, fanout } => {
                format!("按下游链深 × {chain} + 直接下游数 × {fanout} 排队")
            }
        }
    }

    pub fn rule(self) -> String {
        match self {
            Self::RosterOrder => "ARIS breaks ties between eligible lanes in roster order.".into(),
            Self::LongestDependencyChain => "ARIS starts eligible lanes with the deepest remaining chain of downstream work first. Declare each actual input dependency so the scheduler can expose that chain.".into(),
            Self::MostDirectDependents => "ARIS starts eligible lanes that unblock the most direct consumers first. Give reusable inputs one owner and connect the workers that consume them.".into(),
            Self::Weighted { chain, fanout } => format!("ARIS ranks eligible lanes by {chain} × remaining downstream chain depth + {fanout} × number of direct consumers. Declare actual dependencies so workers that unlock more work can start promptly."),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct ActivePolicy {
    pub policy: SchedulingPolicy,
    pub revision: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PolicyComparison {
    pub candidate: SchedulingPolicy,
    pub baseline_ms: u64,
    pub candidate_ms: u64,
    pub historical_regressions: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PolicyEvaluation {
    pub outcome: String,
    pub baseline: SchedulingPolicy,
    pub active: SchedulingPolicy,
    pub revision: u64,
    pub observations: usize,
    pub comparisons: Vec<PolicyComparison>,
    pub reason: String,
}

/// Inputs persist only graph indices, collision groups and measured lane times.
#[derive(Debug, Serialize, Deserialize)]
struct ReplayCase {
    slots: usize,
    lanes: Vec<ReplayLane>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ReplayLane {
    dependencies: Vec<usize>,
    files: Vec<usize>,
    duration_ms: u64,
}

#[derive(Default, Serialize, Deserialize)]
struct PolicyState {
    active: ActivePolicy,
    cases: Vec<ReplayCase>,
    last_receipt: Option<(String, PolicyEvaluation)>,
}

fn state(dir: &Path) -> Result<PolicyState> {
    match File::open(dir.join("state.json")) {
        Ok(file) => Ok(serde_json::from_reader(file)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(PolicyState::default()),
        Err(e) => Err(e.into()),
    }
}

fn lock_file(dir: &Path) -> Result<File> {
    fs::create_dir_all(dir)?;
    Ok(OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join("state.lock"))?)
}

pub fn clear(runtime: &Runtime) -> Result<()> {
    let dir = runtime.outer_home.join("learn/workflow-scheduling-v1");
    let lock = lock_file(&dir)?;
    fs2::FileExt::lock_exclusive(&lock)?;
    let receipts = dir.join("evaluations");
    if receipts.try_exists()? {
        fs::remove_dir_all(receipts)?;
    }
    let path = dir.join("state.json");
    if path.try_exists()? {
        fs::remove_file(path)?;
    }
    Ok(())
}

pub fn active(runtime: &Runtime) -> Result<ActivePolicy> {
    if !runtime.config.learn.enabled || !runtime.settings.read().memory_inject {
        return Ok(ActivePolicy::default());
    }
    let dir = runtime.outer_home.join("learn/workflow-scheduling-v1");
    if !dir.try_exists()? {
        return Ok(ActivePolicy::default());
    }
    let lock = lock_file(&dir)?;
    fs2::FileExt::lock_shared(&lock)?;
    Ok(state(&dir)?.active)
}

pub fn for_execution(runtime: &Runtime) -> ActivePolicy {
    active(runtime).unwrap_or_else(|err| {
        tracing::warn!(error=%err, "Workflow scheduling policy is unavailable; using roster order");
        ActivePolicy::default()
    })
}

fn dependencies(agents: &[&TeamAgent]) -> Vec<Vec<usize>> {
    let indices: HashMap<_, _> = agents
        .iter()
        .enumerate()
        .map(|(i, a)| (a.id.as_str(), i))
        .collect();
    agents
        .iter()
        .map(|a| {
            a.depends_on
                .iter()
                .filter_map(|id| indices.get(id.as_str()).copied())
                .collect()
        })
        .collect()
}

// Reverse topological order gives each prerequisite its downstream chain depth.
fn ranks(policy: SchedulingPolicy, dependencies: &[Vec<usize>]) -> Vec<usize> {
    let mut rank = vec![0; dependencies.len()];
    if policy == SchedulingPolicy::RosterOrder {
        return rank;
    }
    let mut downstream_count = vec![0usize; dependencies.len()];
    for deps in dependencies {
        for &id in deps {
            downstream_count[id] += 1;
        }
    }
    if policy == SchedulingPolicy::MostDirectDependents {
        return downstream_count;
    }
    let mut unresolved = downstream_count.clone();
    let mut leaves: Vec<_> = downstream_count
        .iter()
        .enumerate()
        .filter_map(|(i, &count)| (count == 0).then_some(i))
        .collect();
    while let Some(i) = leaves.pop() {
        for &prereq in &dependencies[i] {
            rank[prereq] = rank[prereq].max(rank[i] + 1);
            unresolved[prereq] -= 1;
            if unresolved[prereq] == 0 {
                leaves.push(prereq);
            }
        }
    }
    if let SchedulingPolicy::Weighted { chain, fanout } = policy {
        for (score, direct) in rank.iter_mut().zip(downstream_count) {
            *score = usize::saturating_mul(*score, usize::from(chain))
                .saturating_add(direct.saturating_mul(usize::from(fanout)));
        }
    }
    rank
}

pub(crate) fn lane_ranks(team: &Team, policy: SchedulingPolicy) -> HashMap<&str, usize> {
    if policy == SchedulingPolicy::RosterOrder {
        return HashMap::new();
    }
    let agents: Vec<_> = admin_workers(team)
        .filter(|a| selected_for_run(team, a) && matches!(a.status.as_str(), "pending" | "running"))
        .collect();
    let ranks = ranks(policy, &dependencies(&agents));
    agents
        .iter()
        .zip(ranks)
        .map(|(agent, r)| (agent.id.as_str(), r))
        .collect()
}

fn timestamp(at: &str) -> Result<i64> {
    Ok(DateTime::parse_from_rfc3339(at)?.timestamp_millis())
}

impl ReplayCase {
    fn from_run(team: &Team) -> Result<Self> {
        ensure!(
            team.status == "completed",
            "排程评测需要一轮正常完成的原生执行。"
        );
        let from = team
            .events
            .iter()
            .rposition(|e| e.kind == "started")
            .context("本轮没有团队调度开始事件。")?;
        let events = &team.events[from..];
        let end = events
            .iter()
            .position(|e| e.kind == "finished")
            .context("本轮还没有团队调度完成事件。")?;
        ensure!(
            events[end + 1..]
                .iter()
                .all(|e| matches!(e.kind.as_str(), "checkpoint" | "workflow_review")),
            "团队完成记录之后还有新的工作；旧的泳道时长不属于这轮任务。"
        );
        let start_at = timestamp(&events[0].at)?;
        let end_at = timestamp(&events[end].at)?;
        let events = &events[..=end];
        ensure!(
            !events.iter().any(|e| matches!(
                e.kind.as_str(),
                "agent_error"
                    | "error"
                    | "final_review"
                    | "tunnel_resumed"
                    | "cowork_turn_started"
                    | "cowork_turn_finished"
            )),
            "本轮包含失败、协作重派或终审重入，静态依赖图无法完整重放。"
        );
        for m in team.tunnels.iter().flat_map(|t| &t.messages) {
            if m.expects_reply || m.kind == "cowork_invite" {
                let at = timestamp(&m.at)?;
                ensure!(
                    at < start_at || at > end_at,
                    "本轮含依赖对方答复的动态协作，需用保留收发时序的评测。"
                );
            }
        }
        let agents: Vec<_> = admin_workers(team)
            .filter(|a| selected_for_run(team, a))
            .collect();
        let slots = team
            .execution_budget
            .as_ref()
            .map_or(8, |b| b.max_parallel.min(8));
        ensure!(
            slots > 0 && agents.len() > 1,
            "本轮缺少可比较的多个泳道或执行槽。"
        );
        let mut intervals: HashMap<&str, (i64, Option<i64>)> = HashMap::new();
        for e in events {
            match e.kind.as_str() {
                "agent_started" => {
                    let id = e.agent_id.as_deref().context("泳道开工缺少标识。")?;
                    ensure!(
                        intervals.insert(id, (timestamp(&e.at)?, None)).is_none(),
                        "同一泳道本轮重复开工，保留原策略。"
                    );
                }
                "agent_finished" => {
                    ensure!(e.message.ends_with(": completed"), "泳道没有成功完成。");
                    let id = e.agent_id.as_deref().context("泳道结束缺少标识。")?;
                    let (_, finish) = intervals
                        .get_mut(id)
                        .context("泳道结束缺少对应的开工记录。")?;
                    ensure!(
                        finish.replace(timestamp(&e.at)?).is_none(),
                        "泳道有多次完成记录。"
                    );
                }
                _ => {}
            }
        }
        ensure!(
            intervals.len() == agents.len(),
            "本轮只执行了部分角色，缺少整图的可比时间。"
        );
        let dependencies = dependencies(&agents);
        let mut file_ids = HashMap::new();
        let mut lanes = Vec::new();
        let mut bounds = Vec::new();
        for (i, a) in agents.iter().enumerate() {
            let &(start, Some(finish)) = intervals.get(a.id.as_str()).context("缺少执行记录。")?
            else {
                bail!("泳道缺少完成时间。")
            };
            ensure!(
                finish > start && start >= start_at && finish <= end_at,
                "泳道时间不足以重放。"
            );
            bounds.push((start, finish));
            for &dep in &dependencies[i] {
                ensure!(
                    intervals
                        .get(agents[dep].id.as_str())
                        .and_then(|v| v.1)
                        .is_some_and(|e| e <= start),
                    "观察到的实际执行顺序与依赖关系不一致。"
                );
            }
            let files = a
                .files
                .iter()
                .map(|path| {
                    let next = file_ids.len();
                    *file_ids.entry(path.to_lowercase()).or_insert(next)
                })
                .collect();
            lanes.push(ReplayLane {
                dependencies: dependencies[i].clone(),
                files,
                duration_ms: (finish - start) as u64,
            });
        }
        let contended = lanes.iter().enumerate().any(|(i, lane)| {
            lanes[..i]
                .iter()
                .any(|other| other.files.iter().any(|f| lane.files.contains(f)))
        });
        ensure!(
            agents.len() > slots || contended,
            "可用并行槽覆盖全部角色，泳道间也没有共享的输出文件，缺少排队比较。"
        );
        for (i, &(start, _)) in bounds.iter().enumerate() {
            let concurrent: Vec<_> = bounds
                .iter()
                .enumerate()
                .filter(|(_, (s, e))| *s <= start && start < *e)
                .map(|(j, _)| j)
                .collect();
            ensure!(
                concurrent.len() <= slots,
                "观察到的并发泳道数与记录的执行预算不一致。"
            );
            ensure!(
                concurrent
                    .into_iter()
                    .filter(|&j| j != i)
                    .all(|j| lanes[j].files.iter().all(|id| !lanes[i].files.contains(id))),
                "共享文件的实际占用时间重叠，当前文件声明不足以重放。"
            );
        }
        Ok(Self { slots, lanes })
    }

    /// Same eligibility/order rules used by ready_wave, with a virtual clock.
    fn makespan(&self, policy: SchedulingPolicy) -> Result<u64> {
        let deps: Vec<_> = self.lanes.iter().map(|l| l.dependencies.clone()).collect();
        ensure!(
            self.slots > 0 && deps.iter().flatten().all(|&id| id < deps.len()),
            "Invalid scheduling replay"
        );
        let ranks = ranks(policy, &deps);
        let mut order: Vec<_> = (0..deps.len()).collect();
        order.sort_by_key(|&i| std::cmp::Reverse(ranks[i]));
        let mut finished = vec![false; deps.len()];
        let mut running = HashMap::<usize, u64>::new();
        let mut time = 0u64;
        while finished.iter().any(|f| !f) {
            let mut claims: HashSet<usize> = running
                .keys()
                .flat_map(|&i| self.lanes[i].files.iter().copied())
                .collect();
            for &i in &order {
                if running.len() >= self.slots {
                    break;
                }
                let lane = &self.lanes[i];
                if finished[i]
                    || running.contains_key(&i)
                    || lane.dependencies.iter().any(|&id| !finished[id])
                    || lane.files.iter().any(|id| claims.contains(id))
                {
                    continue;
                }
                claims.extend(lane.files.iter().copied());
                running.insert(
                    i,
                    time.checked_add(lane.duration_ms)
                        .context("Replay clock overflow")?,
                );
            }
            time = *running
                .values()
                .min()
                .context("Invalid scheduling replay: no eligible lane")?;
            running.retain(|&i, end| {
                if *end == time {
                    finished[i] = true;
                    false
                } else {
                    true
                }
            });
        }
        Ok(time)
    }
}

fn settle(
    state: &mut PolicyState,
    input: ReplayCase,
    candidate: Option<SchedulingPolicy>,
) -> Result<PolicyEvaluation> {
    use SchedulingPolicy::*;
    let baseline = state.active.policy;
    let candidates: Vec<_> = candidate
        .map(|p| vec![p])
        .unwrap_or_else(|| vec![RosterOrder, LongestDependencyChain, MostDirectDependents]);
    let baseline_ms = input.makespan(baseline)?;
    let mut comparisons = Vec::new();
    let mut best = baseline;
    let mut best_time = baseline_ms;
    for candidate in candidates {
        let candidate_ms = input.makespan(candidate)?;
        let mut historical_regressions = 0;
        for prior in &state.cases {
            if prior.makespan(candidate)? > prior.makespan(baseline)? {
                historical_regressions += 1;
            }
        }
        if candidate_ms < best_time && historical_regressions == 0 {
            best = candidate;
            best_time = candidate_ms;
        }
        comparisons.push(PolicyComparison {
            candidate,
            baseline_ms,
            candidate_ms,
            historical_regressions,
        });
    }
    let changed = best != baseline;
    if changed {
        state.active = ActivePolicy {
            policy: best,
            revision: state
                .active
                .revision
                .checked_add(1)
                .context("Policy revision overflow")?,
        };
    }
    let observations = state.cases.len() + 1;
    // Bounded set of recent regression cases, with permanent idempotent receipts.
    if state.cases.len() >= crate::sim::HISTORY_LIMIT {
        state.cases.remove(0);
    }
    state.cases.push(input);
    Ok(PolicyEvaluation {
        outcome: if changed { "activated" } else { "retained" }.into(),
        baseline,
        active: best,
        revision: state.active.revision,
        observations,
        comparisons,
        reason: if changed {
            "在本次泳道时长、依赖和文件占用相同的排程重放里用时缩短，已保留样本没有退步。"
        } else {
            "候选未缩短本案的重放排程，或在保留的对照样本出现退步。"
        }
        .into(),
    })
}

pub fn assess(
    runtime: &Runtime,
    review_id: &str,
    team: &Team,
    candidate: Option<SchedulingPolicy>,
) -> Result<PolicyEvaluation> {
    let active = active(runtime)?;
    let unchanged = |outcome: &str, reason: String| PolicyEvaluation {
        outcome: outcome.into(),
        baseline: active.policy,
        active: active.policy,
        revision: active.revision,
        observations: 0,
        comparisons: vec![],
        reason,
    };
    if !runtime.learn.generation_enabled()
        || !runtime.settings.read().memory_inject
        || !runtime.config.learn.enabled
    {
        return Ok(unchanged(
            "disabled",
            "当前记忆设置未启用策略学习和应用。".into(),
        ));
    }
    let dir = runtime.outer_home.join("learn/workflow-scheduling-v1");
    let case = ReplayCase::from_run(team);
    let id = format!("{:x}", Sha256::digest(review_id.as_bytes()));
    let lock = lock_file(&dir)?;
    fs2::FileExt::lock_exclusive(&lock)?;
    let mut state = state(&dir)?;
    let receipts = dir.join("evaluations");
    if let Some((last, report)) = &state.last_receipt {
        let path = receipts.join(format!("{last}.json"));
        if !path.try_exists()? {
            atomic_write(&path, &serde_json::to_vec(report)?)?;
        }
    }
    let path = receipts.join(format!("{id}.json"));
    if path.try_exists()? {
        return Ok(serde_json::from_reader(File::open(path)?)?);
    }
    let report = match case {
        Err(reason) => PolicyEvaluation {
            baseline: state.active.policy,
            active: state.active.policy,
            revision: state.active.revision,
            ..unchanged("insufficient_evidence", reason.to_string())
        },
        Ok(input) => {
            let r = settle(&mut state, input, candidate)?;
            // Write ahead the last receipt in the same transaction as activation.
            // Recovery materializes it before any subsequent decision can commit.
            state.last_receipt = Some((id, r.clone()));
            atomic_write(&dir.join("state.json"), &serde_json::to_vec(&state)?)?;
            r
        }
    };
    atomic_write(&path, &serde_json::to_vec(&report)?)?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::{TeamEvent, TeamSpec};
    use crate::tests::RuntimeFixture;
    use serde_json::json;

    fn fixture() -> RuntimeFixture {
        RuntimeFixture::new(crate::SessionSettings::default(), true)
    }

    fn event(ms: i64, kind: &str, id: Option<&str>) -> TeamEvent {
        TeamEvent {
            at: chrono::DateTime::from_timestamp_millis(ms)
                .unwrap()
                .to_rfc3339(),
            agent_id: id.map(str::to_string),
            kind: kind.into(),
            message: id.map_or_else(String::new, |s| format!("{s}: completed")),
        }
    }

    fn completed(stamps: [(i64, i64); 4]) -> Team {
        let spec:TeamSpec=serde_json::from_value(json!({
            "name":"scheduler","goal":"Run graph",
            "agents":(["a","b","c","d"].into_iter().map(|id|json!({"id":id,"name":id,"profile_id":"m",
                "depends_on":if id=="d"{vec!["c"]}else{vec![]},"status":"completed"})).collect::<Vec<_>>())
        })).unwrap();
        let mut events = vec![event(0, "started", None)];
        for (i, (start, finish)) in stamps.into_iter().enumerate() {
            let id = &spec.agents[i].id;
            events.push(event(start, "agent_started", Some(id)));
            events.push(event(finish, "agent_finished", Some(id)));
        }
        events.sort_by_key(|e| timestamp(&e.at).unwrap());
        events.push(event(100, "finished", None));
        serde_json::from_value(json!({"id":"different-separate-reviews","name":spec.name,"goal":spec.goal,"agents":spec.agents,"events":events,"status":"completed","files":[],"handoffs":[],"checkpoints":[],"created_at":"",
            "execution_budget":{"agent_ids":["a","b","c","d"],"max_agent_runs":4,"max_steps_per_agent":12,"max_tool_calls_per_agent":32,"max_tokens_per_step":4096,"max_parallel":2,"max_runtime_secs":900}
        })).unwrap()
    }

    #[test]
    fn activates_measured_improvement_once_and_recovers_receipt_across_calls() {
        let f = fixture();
        let good = completed([(1, 6), (1, 6), (6, 11), (11, 21)]);
        let r = assess(&f.runtime, "first", &good, None).unwrap();
        assert_eq!(r.outcome, "activated");
        assert_eq!(r.baseline, SchedulingPolicy::RosterOrder);
        assert_eq!(r.active, SchedulingPolicy::LongestDependencyChain);
        assert_eq!(r.revision, 1);
        let best = r
            .comparisons
            .iter()
            .find(|c| c.candidate == r.active)
            .unwrap();
        assert_eq!((best.baseline_ms, best.candidate_ms), (20, 15));
        let stored = active(&f.runtime).unwrap();
        assert_eq!(stored.policy, r.active);
        // Simulate a stop after atomic activation and before receipt materialization.
        let dir = f.runtime.outer_home.join("learn/workflow-scheduling-v1");
        let receipt = dir.join(format!("evaluations/{:x}.json", Sha256::digest(b"first")));
        fs::remove_file(&receipt).unwrap();
        let retried = assess(&f.runtime, "first", &good, None).unwrap();
        assert_eq!(retried.outcome, "activated");
        assert_eq!(retried.revision, 1);
        assert!(receipt.exists());
        assert_eq!(state(&dir).unwrap().cases.len(), 1);
        let mut next_team = good.clone();
        for worker in &mut next_team.agents {
            worker.status = "pending".into();
        }
        assert_eq!(lane_ranks(&next_team, stored.policy)["c"], 1);
        assert!(stored.policy.rule().contains("downstream"));
        let no_gain = assess(&f.runtime, "next", &good, None).unwrap();
        assert_eq!(no_gain.outcome, "retained");
        assert_eq!(no_gain.revision, 1);
        clear(&f.runtime).unwrap();
        assert!(!receipt.exists());
        assert_eq!(active(&f.runtime).unwrap().revision, 0);
        let mut overlap = completed([(1, 6), (1, 6), (1, 11), (11, 21)]);
        assert!(ReplayCase::from_run(&overlap).is_err());
        overlap = completed([(1, 6), (1, 6), (6, 11), (11, 21)]);
        overlap.agents[0].files = vec!["shared.md".into()];
        overlap.agents[1].files = vec!["SHARED.MD".into()];
        assert!(ReplayCase::from_run(&overlap).is_err());
    }

    #[test]
    fn candidate_losing_on_a_prior_case_cannot_promote() {
        let f = fixture();
        let old = completed([(1, 2), (1, 5), (2, 3), (3, 4)]);
        let r = assess(&f.runtime, "old", &old, None).unwrap();
        assert_eq!(r.outcome, "retained");
        let fresh = completed([(1, 6), (1, 6), (6, 11), (11, 21)]);
        let r = assess(
            &f.runtime,
            "fresh",
            &fresh,
            Some(SchedulingPolicy::LongestDependencyChain),
        )
        .unwrap();
        assert_eq!(r.outcome, "retained");
        assert_eq!(r.comparisons[0].historical_regressions, 1);
        assert!(r.comparisons[0].candidate_ms < r.comparisons[0].baseline_ms);
        assert_eq!(active(&f.runtime).unwrap().revision, 0);
    }

    #[test]
    fn reviewer_weights_change_the_running_queue_and_require_replay_evidence() {
        let chain = SchedulingPolicy::Weighted {
            chain: 2,
            fanout: 1,
        };
        let breadth = SchedulingPolicy::Weighted {
            chain: 1,
            fanout: 2,
        };
        let deps = vec![
            vec![],
            vec![],
            vec![0],
            vec![2],
            vec![3, 1],
            vec![1],
            vec![1],
        ];
        assert_eq!(ranks(chain, &deps)[..2], [7, 5]);
        assert_eq!(ranks(breadth, &deps)[..2], [5, 7]);
        assert_eq!(
            serde_json::to_value(chain).unwrap(),
            json!({"weighted":{"chain":2,"fanout":1}})
        );
        for invalid in [
            json!({"weighted":{"chain":256,"fanout":1}}),
            json!({"weighted":{"fanout":1}}),
            json!({"weighted":{"chain":-1,"fanout":0}}),
        ] {
            assert!(serde_json::from_value::<SchedulingPolicy>(invalid).is_err());
        }
        let f = fixture();
        let mut t = completed([(1, 6), (1, 6), (6, 11), (11, 21)]);
        let decision = assess(&f.runtime, "weighted", &t, Some(chain)).unwrap();
        assert_eq!(decision.active, chain);
        let stored = for_execution(&f.runtime);
        assert_eq!(stored.policy, chain);
        assert!(stored.policy.rule().contains("2 × remaining"));
        assert!(stored.policy.display_name().contains("链深 × 2"));
        for a in &mut t.agents {
            a.status = "pending".into();
        }
        assert_eq!(lane_ranks(&t, stored.policy)["c"], 3);
    }

    #[test]
    fn excludes_reentered_lanes_missing_or_out_of_boundary_evidence() {
        let f = fixture();
        let mut t = completed([(1, 6), (1, 6), (6, 11), (11, 21)]);
        t.events.push(event(101, "admin_answered", None));
        assert_eq!(
            assess(&f.runtime, "later-direct-answer", &t, None)
                .unwrap()
                .outcome,
            "insufficient_evidence"
        );
        t.events.pop();
        t.events.insert(2, event(5, "agent_started", Some("a")));
        assert_eq!(
            assess(&f.runtime, "reentered", &t, None).unwrap().outcome,
            "insufficient_evidence"
        );
        t.events.remove(2);
        t.events.retain(|e| e.agent_id.as_deref() != Some("c"));
        assert_eq!(
            assess(&f.runtime, "incomplete", &t, None).unwrap().outcome,
            "insufficient_evidence"
        );
        assert_eq!(active(&f.runtime).unwrap().revision, 0);
    }

    #[test]
    fn replay_preserves_file_reservations_and_uses_only_the_latest_completed_run() {
        let f = fixture();
        let mut t = completed([(1, 6), (1, 6), (6, 11), (11, 21)]);
        t.agents[0].files = vec!["article.md".into()];
        t.agents[2].files = vec!["ARTICLE.md".into()];
        t.execution_budget.as_mut().unwrap().max_parallel = 8;
        let mut prior = t.events.clone();
        prior.push(event(5, "agent_started", Some("a")));
        prior.extend(t.events);
        t.events = prior;
        let r = assess(
            &f.runtime,
            "files",
            &t,
            Some(SchedulingPolicy::LongestDependencyChain),
        )
        .unwrap();
        assert_eq!(r.outcome, "activated");
        assert_eq!(r.comparisons[0].candidate_ms, 15);
        f.runtime.settings.write().memory_inject = false;
        assert_eq!(
            active(&f.runtime).unwrap().policy,
            SchedulingPolicy::RosterOrder
        );
        assert_eq!(
            assess(&f.runtime, "off", &t, None).unwrap().outcome,
            "disabled"
        );
    }
}
