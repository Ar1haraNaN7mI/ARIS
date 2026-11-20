//! Write-ahead identities for the narrow gap between a native terminal commit
//! and publication of its review job. Recovery only visits these files.
use crate::EventStore;
use crate::{
    harness::{Team, TeamEvent},
    settings::atomic_write,
    workflow_reviewer, RunState, Runtime, TaskState,
};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Serialize, Deserialize)]
enum Intent {
    Turn {
        task: String,
        session: String,
        run: String,
        chat: Option<String>,
        scheduled: bool,
    },
    IdleCancel {
        task: String,
        session: String,
        run: String,
    },
    BeforeStart {
        task: Box<crate::TaskRecord>,
        outcome: TaskState,
        at: chrono::DateTime<chrono::Utc>,
        reason: String,
        after: u64,
    },
    Team {
        team: Box<Team>,
        marker_at: String,
        marker_kind: String,
    },
}

fn dir(runtime: &Runtime) -> PathBuf {
    runtime.workspace_outer.join("review-intents")
}
fn path(runtime: &Runtime, key: &str) -> PathBuf {
    dir(runtime).join(format!("{:x}.json", Sha256::digest(key.as_bytes())))
}

pub(crate) fn is_team_end(e: &TeamEvent) -> bool {
    matches!(
        e.kind.as_str(),
        "finished" | "cancelled" | "error" | "interrupted" | "admin_answered" | "admin_stopped"
    )
}

fn team_key(team: &Team, end: &TeamEvent) -> String {
    format!("team/{}/{}/{}", team.id, end.kind, end.at)
}

pub(crate) fn stage_team(runtime: &Runtime, team: &Team, new_events: &[TeamEvent]) -> Result<()> {
    if !runtime.learn.generation_enabled() {
        return Ok(());
    }
    let Some(marker) = new_events.iter().rev().find(|e| is_team_end(e)) else {
        return Ok(());
    };
    let mut snapshot = team.clone();
    snapshot.checkpoints.clear();
    let intent = Intent::Team {
        team: Box::new(snapshot),
        marker_at: marker.at.clone(),
        marker_kind: marker.kind.clone(),
    };
    atomic_write(
        &path(runtime, &team_key(team, marker)),
        &serde_json::to_vec(&intent)?,
    )
}

pub(crate) fn ack_team(runtime: &Runtime, team: &Team) {
    if let Some(marker) = team.events.iter().rev().find(|e| is_team_end(e)) {
        clear(&path(runtime, &team_key(team, marker)));
    }
}

pub(crate) fn stage_turn(
    runtime: &Runtime,
    task: &str,
    session: &str,
    run: &str,
    chat: Option<&str>,
) -> Result<()> {
    if !runtime.learn.generation_enabled()
        || runtime.tasks.task(task).is_none_or(|t| t.owner.is_some())
    {
        return Ok(());
    }
    let intent = Intent::Turn {
        task: task.into(),
        session: session.into(),
        run: run.into(),
        chat: chat.map(str::to_owned),
        scheduled: runtime.scheduler.is_dispatching(task),
    };
    atomic_write(&path(runtime, run), &serde_json::to_vec(&intent)?)
}

pub(crate) fn has_turn(runtime: &Runtime, run: &str) -> bool {
    path(runtime, run).is_file()
}

pub(crate) fn stage_idle_cancel(
    runtime: &Runtime,
    task: &crate::TaskRecord,
    run: &crate::RunRecord,
) -> Result<()> {
    if !runtime.learn.generation_enabled() || task.owner.is_some() {
        return Ok(());
    }
    let Some(session) = &task.session_id else {
        return Ok(());
    };
    atomic_write(
        &path(runtime, &run.id),
        &serde_json::to_vec(&Intent::IdleCancel {
            task: task.id.clone(),
            session: session.clone(),
            run: run.id.clone(),
        })?,
    )
}

pub(crate) fn finish_before_start(
    runtime: &Arc<Runtime>,
    id: &str,
    outcome: TaskState,
    reason: &str,
) -> Result<crate::TaskRecord> {
    let at = chrono::Utc::now();
    let task = runtime.tasks.task(id).context("Scheduler task missing")?;
    let file = path(runtime, &format!("start/{id}/{at}"));
    if runtime.learn.generation_enabled() && task.owner.is_none() {
        let intent = Intent::BeforeStart {
            task: Box::new(task),
            outcome,
            at,
            reason: reason.into(),
            after: runtime.events.latest_sequence(),
        };
        atomic_write(&file, &serde_json::to_vec(&intent)?)?;
    }
    let finished = runtime.tasks.transition_task_at(
        id,
        outcome,
        at,
        (outcome == TaskState::Failed).then_some(reason),
    )?;
    let (event, source) = if outcome == TaskState::Failed {
        ("task.failed", crate::EventSource::Scheduler)
    } else {
        ("task.cancelled", crate::EventSource::User)
    };
    let _ = runtime.record_event(
        crate::EventEnvelope::new(event, serde_json::json!({"reason":reason}))
            .with_source(source)
            .with_task(id),
    );
    let uid = workflow_reviewer::enqueue_unstarted(runtime, &finished, reason, at, outcome)?;
    clear(&file);
    runtime.review_queued(Ok(uid));
    Ok(finished)
}

fn clear(file: &Path) {
    if let Err(error) = fs::remove_file(file) {
        if error.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(%error, intent=%file.display(),"Could not clear workflow review intent");
        }
    }
}

pub(crate) fn recover(runtime: &Arc<Runtime>) -> Result<()> {
    let directory = dir(runtime);
    if !directory.try_exists()? {
        return Ok(());
    }
    for entry in fs::read_dir(directory)? {
        let file = entry?.path();
        if file.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        recover_file(runtime, &file);
    }
    Ok(())
}

pub(crate) fn recover_turn(runtime: &Arc<Runtime>, run: &str) {
    recover_file(runtime, &path(runtime, run));
}

fn recover_file(runtime: &Arc<Runtime>, file: &Path) {
    if !runtime.learn.generation_enabled() {
        clear(file);
        return;
    }
    let result = (|| -> Result<bool> {
        let intent: Intent = match fs::File::open(&file) {
            Ok(f) => serde_json::from_reader(f)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        let uid = match intent {
            Intent::BeforeStart {
                task,
                outcome,
                at,
                reason,
                after,
            } => {
                let current = runtime
                    .tasks
                    .task(&task.id)
                    .context("Scheduler task missing")?;
                let committed = current.state == outcome && current.updated_at == at
                    || runtime.events.read_after(after)?.iter().any(|e| {
                        e.task_id.as_deref() == Some(&task.id)
                            && e.event_type == "task.state_changed"
                            && e.payload["updated_at"] == serde_json::json!(at)
                            && e.payload["state"] == serde_json::json!(outcome)
                    });
                if !committed {
                    return Ok(false);
                }
                workflow_reviewer::enqueue_unstarted(runtime, &task, &reason, at, outcome)?
            }
            Intent::IdleCancel { task, session, run } => {
                let task = runtime
                    .tasks
                    .task(&task)
                    .context("Review intent task missing")?;
                if task.state != TaskState::Cancelled {
                    return Ok(false);
                }
                let run = runtime
                    .tasks
                    .run(&run)
                    .context("Review intent run missing")?;
                workflow_reviewer::enqueue_task_run(
                    runtime,
                    &task,
                    &run,
                    &session,
                    TaskState::Cancelled,
                )?
            }
            Intent::Team {
                team,
                marker_at,
                marker_kind,
            } => {
                // The team's state lock serializes this check with its commit.
                match runtime
                    .teams
                    .has_terminal_event(&team.id, &marker_at, &marker_kind)?
                {
                    Some(true) => {}
                    Some(false) => return Ok(true),
                    None => return Ok(false),
                }
                workflow_reviewer::enqueue_team(runtime, &team)?
            }
            Intent::Turn {
                task,
                session,
                run,
                chat,
                scheduled,
            } => {
                let attempt = runtime
                    .tasks
                    .run(&run)
                    .context("Review intent run missing")?;
                let t = runtime
                    .tasks
                    .task(&task)
                    .context("Review intent task missing")?;
                let latest = runtime
                    .tasks
                    .latest_run_for_task(&task)
                    .is_some_and(|r| r.id == run);
                if latest
                    && matches!(t.state, TaskState::Paused | TaskState::Queued)
                    && matches!(
                        attempt.state,
                        RunState::Paused | RunState::Failed | RunState::Cancelled
                    )
                {
                    return Ok(true);
                }
                let outcome = match attempt.state {
                    RunState::Completed => TaskState::Completed,
                    RunState::Cancelled => TaskState::Cancelled,
                    RunState::Failed
                        if scheduled && attempt.attempt < t.retry_policy.max_attempts =>
                    {
                        return Ok(true)
                    }
                    RunState::Failed if latest && t.state == TaskState::Queued => return Ok(true),
                    RunState::Failed => TaskState::Failed,
                    RunState::Paused if latest && t.state == TaskState::Cancelled => {
                        TaskState::Cancelled
                    }
                    RunState::Paused => return Ok(true),
                    _ => return Ok(false),
                };
                if let Some(turn) = chat {
                    workflow_reviewer::enqueue_chat_outcome(
                        runtime,
                        &session,
                        &turn,
                        &task,
                        Some(outcome),
                    )?
                } else {
                    workflow_reviewer::enqueue_task_run(runtime, &t, &attempt, &session, outcome)?
                }
            }
        };
        runtime.review_queued(Ok(uid));
        Ok(true)
    })();
    match result {
        Ok(true) => clear(&file),
        Ok(false) => {}
        Err(error) => {
            tracing::warn!(%error,intent=%file.display(),"Could not recover this workflow review intent")
        }
    }
}
