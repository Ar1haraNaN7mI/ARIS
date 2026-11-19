//! Native subagent lifecycle tests use only a controlled loopback model.
use super::*;
use aris_protocol::RunState;
use serde_json::{json, Value};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::oneshot,
};

struct Request {
    body: Value,
    reply: oneshot::Sender<String>,
}

async fn fixture() -> (
    crate::tests::RuntimeFixture,
    mpsc::Receiver<Request>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (requests, rx) = mpsc::channel(16);
    let server = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let requests = requests.clone();
            tokio::spawn(async move {
                let mut bytes = Vec::new();
                let mut buffer = [0; 4096];
                let (offset, length) = loop {
                    let count = socket.read(&mut buffer).await.unwrap();
                    assert!(count > 0);
                    bytes.extend_from_slice(&buffer[..count]);
                    if let Some(offset) = bytes.windows(4).position(|value| value == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..offset]);
                        let length: usize = headers
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|value| value.trim().parse().unwrap())
                            })
                            .unwrap();
                        break (offset + 4, length);
                    }
                };
                while bytes.len() < offset + length {
                    let count = socket.read(&mut buffer).await.unwrap();
                    assert!(count > 0);
                    bytes.extend_from_slice(&buffer[..count]);
                }
                let body = serde_json::from_slice(&bytes[offset..offset + length]).unwrap();
                let (reply, receive) = oneshot::channel();
                requests.send(Request { body, reply }).await.unwrap();
                if let Ok(text) = receive.await {
                    let payload = json!({"choices":[{"message":{"role":"assistant","content":text},"finish_reason":"stop"}]}).to_string();
                    let _ = socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", payload.len(), payload).as_bytes()).await;
                }
            });
        }
    });
    let fixture = crate::tests::RuntimeFixture::new(crate::SessionSettings::default(), false);
    let runtime = &fixture.runtime;
    fs::write(
        runtime.outer_home.join("credentials.env"),
        "KEI_API_KEY=local-subagent-fixture\n",
    )
    .unwrap();
    let mut kei = crate::kei::load_kei_config(&runtime.outer_home, &runtime.config.kei).unwrap();
    let mut profile =
        crate::ModelProfileConfig::new(crate::ModelFamily::Gpt, "local-subagent-fixture");
    profile.id = "native".into();
    profile.base_url = Some(format!("http://{address}/v1"));
    kei.profiles.push(profile.clone());
    crate::kei::save_kei_config(&runtime.outer_home, &kei).unwrap();
    runtime.llm.replace_config(
        profile
            .resolve(&kei, &runtime.llm.config(), &runtime.outer_home)
            .unwrap(),
    );
    (fixture, rx, server)
}

fn worker(id: &str, deps: &[&str]) -> TeamAgent {
    serde_json::from_value(json!({"id":id,"name":id,"role":"analyst","profile_id":"native","prompt":format!("Answer as {id}"),"depends_on":deps})).unwrap()
}

async fn request(rx: &mut mpsc::Receiver<Request>) -> Request {
    tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
        .await
        .unwrap()
        .unwrap()
}

async fn settled(runtime: &Runtime, id: &str) -> Team {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let team = runtime.teams.get(id).unwrap();
            if !matches!(team.status.as_str(), "running" | "cancelling" | "planning")
                && runtime.teams.run_lock(id).is_ok()
            {
                return team;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|error| panic!("{error}: {:?}", runtime.teams.get(id)))
}

#[tokio::test]
async fn native_workers_parallel_dependencies_direct_continuation_and_child_cancel() {
    let (fixture, mut rx, server) = fixture().await;
    let runtime = fixture.runtime.clone();
    let team = runtime
        .teams
        .create(
            &runtime,
            TeamSpec {
                name: "native lifecycle".into(),
                goal: "Two independent analyses then a synthesis".into(),
                agents: vec![
                    worker("a", &[]),
                    worker("b", &[]),
                    worker("c", &["a", "b"]),
                    worker("unselected", &[]),
                ],
                files: vec![],
            },
        )
        .unwrap();
    runtime
        .teams
        .mutate(&team.id, |team| {
            team.dispatch_agent_ids = Some(vec!["a".into(), "b".into(), "c".into()]);
            Ok(())
        })
        .unwrap();
    runtime.teams.start(runtime.clone(), &team.id).unwrap();
    let first = request(&mut rx).await;
    let second = request(&mut rx).await;
    assert_eq!(first.body["model"], "local-subagent-fixture");
    assert_eq!(second.body["model"], first.body["model"]);
    let running = runtime.teams.get(&team.id).unwrap();
    assert!(
        running.agents[2].task_id.is_none(),
        "dependent role must not be spawned early"
    );
    assert!(running.agents[3].task_id.is_none());
    for agent in &running.agents[..2] {
        let task = runtime
            .tasks
            .task(agent.task_id.as_deref().unwrap())
            .unwrap();
        assert_eq!(task.session_id.as_deref(), Some(agent.session_id.as_str()));
        assert_eq!(
            task.owner.as_ref().unwrap().subagent_id,
            *agent.subagent_id.as_ref().unwrap()
        );
        assert_eq!(task.owner.as_ref().unwrap().team_id, team.id);
        assert!(runtime.active_agents.lock().contains_key(&task.id));
        assert!(runtime.tasks.latest_run_for_task(&task.id).is_some());
    }
    first.reply.send("analysis first".into()).unwrap();
    second.reply.send("analysis second".into()).unwrap();
    let third = request(&mut rx).await;
    let input = third.body["messages"].as_array().unwrap().last().unwrap()["content"]
        .as_str()
        .unwrap();
    assert!(input.contains("analysis first") && input.contains("analysis second"));
    third.reply.send("synthesis".into()).unwrap();
    let completed = settled(&runtime, &team.id).await;
    assert_eq!(completed.status, "completed");
    assert_eq!(completed.agents[3].status, "pending");
    let ids: HashSet<_> = completed.agents[..3]
        .iter()
        .map(|agent| agent.subagent_id.clone().unwrap())
        .collect();
    assert_eq!(ids.len(), 3);
    let before = completed.agents[0].clone();
    let old_dependency = completed.agents[2].task_id.clone();
    runtime
        .teams
        .chat_message(&team.id, Some("a"), "Explain just your analysis")
        .unwrap();
    runtime.teams.start(runtime.clone(), &team.id).unwrap();
    let continuation = request(&mut rx).await;
    assert!(continuation
        .body
        .to_string()
        .contains("Explain just your analysis"));
    continuation
        .reply
        .send("continued original conversation".into())
        .unwrap();
    let continued = settled(&runtime, &team.id).await;
    let after = &continued.agents[0];
    assert_eq!(after.subagent_id, before.subagent_id);
    assert_eq!(after.session_id, before.session_id);
    assert_ne!(after.task_id, before.task_id);
    assert_ne!(after.run_id, before.run_id);
    assert_eq!(continued.agents[2].task_id, old_dependency);
    assert_eq!(continued.agents[3].status, "pending");
    assert!(continued.agents[3].task_id.is_none());
    assert_eq!(
        runtime
            .tasks
            .task(before.task_id.as_deref().unwrap())
            .unwrap()
            .state,
        TaskState::Completed
    );
    assert!(
        rx.try_recv().is_err(),
        "direct continuation must not resurrect unselected roles"
    );

    runtime
        .teams
        .chat_message(&team.id, Some("a"), "Wait for a cancellation")
        .unwrap();
    runtime.teams.start(runtime.clone(), &team.id).unwrap();
    let cancellation = request(&mut rx).await;
    let active = runtime.teams.get(&team.id).unwrap();
    let task_id = active.agents[0].task_id.as_deref().unwrap();
    assert!(runtime.cancel_active_task(task_id));
    let cancelled = settled(&runtime, &team.id).await;
    drop(cancellation.reply);
    assert_eq!(cancelled.agents[0].status, "cancelled");
    assert_eq!(
        runtime.tasks.task(task_id).unwrap().state,
        TaskState::Cancelled
    );
    assert_eq!(cancelled.agents[2].task_id, old_dependency);
    assert_eq!(cancelled.agents[3].status, "pending");
    assert!(runtime.active_agents.lock().is_empty());
    server.abort();
}

#[tokio::test]
async fn legacy_registry_migration_is_idempotent_and_scheduler_cannot_steal_retries() {
    let (fixture, mut requests, server) = fixture().await;
    let runtime = fixture.runtime.clone();
    let team = runtime
        .teams
        .create(
            &runtime,
            TeamSpec {
                name: "legacy".into(),
                goal: "resume".into(),
                agents: vec![worker("admin", &[]), worker("b", &[])],
                files: vec![],
            },
        )
        .unwrap();
    let mut legacy = team.clone();
    for agent in &mut legacy.agents {
        agent.subagent_id = None;
    }
    runtime.teams.save(&legacy).unwrap();
    let mut tasks = Vec::new();
    for agent in &legacy.agents {
        let mut task = TaskRecord::new_with_retry_policy(
            GoalSpec::new("legacy work"),
            RetryPolicy {
                max_attempts: 3,
                backoff_secs: 0,
                max_backoff_secs: 0,
            },
        )
        .unwrap();
        task.session_id = Some(agent.session_id.clone());
        let task = runtime.tasks.create_task(task).unwrap();
        runtime
            .tasks
            .transition_task(&task.id, TaskState::Running)
            .unwrap();
        let run = runtime.tasks.create_run(&task.id, 1).unwrap();
        runtime
            .tasks
            .transition_run(&run.id, RunState::Failed)
            .unwrap();
        runtime
            .tasks
            .transition_task(&task.id, TaskState::Failed)
            .unwrap();
        tasks.push(task.id);
    }
    runtime.teams.reconcile_tasks(&runtime.tasks).unwrap();
    let migrated = runtime.teams.get(&team.id).unwrap();
    assert_eq!(migrated.agents[0].session_id, legacy.agents[0].session_id);
    assert!(
        migrated.agents[0].subagent_id.is_some(),
        "a user role named admin remains a worker"
    );
    assert_eq!(migrated.agents[0].task_id.as_ref(), Some(&tasks[0]));
    let path = runtime.teams.directory(&team.id).unwrap().join("team.json");
    let modified = fs::metadata(&path).unwrap().modified().unwrap();
    runtime.teams.reconcile_tasks(&runtime.tasks).unwrap();
    assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
    let reopened = Runtime::bootstrap(
        runtime.config.clone(),
        runtime.workspace_root.clone(),
        crate::DeepSeekClient::new(runtime.llm.config()).unwrap(),
        runtime.tools.clone(),
    )
    .unwrap();
    assert_eq!(
        reopened.teams.get(&team.id).unwrap().agents[0].subagent_id,
        migrated.agents[0].subagent_id
    );
    reopened.scheduler.tick_once().await;
    for id in &tasks {
        assert_eq!(reopened.tasks.task(id).unwrap().state, TaskState::Failed);
        assert_eq!(reopened.tasks.latest_run_for_task(id).unwrap().attempt, 1);
    }
    reopened
        .tasks
        .transition_task(&tasks[0], TaskState::Queued)
        .unwrap();
    reopened.scheduler.tick_once().await;
    assert_eq!(
        reopened.tasks.task(&tasks[0]).unwrap().state,
        TaskState::Queued
    );
    assert_eq!(
        reopened
            .tasks
            .latest_run_for_task(&tasks[0])
            .unwrap()
            .attempt,
        1
    );
    assert!(requests.try_recv().is_err());
    let session = reopened
        .sessions
        .get_or_load(&migrated.agents[0].session_id)
        .unwrap();
    let runner = AgentLoop::new(reopened.clone());
    let (tx, _) = mpsc::channel(8);
    assert!(runner
        .run_task(
            tasks[0].clone(),
            session.clone(),
            "bypass".into(),
            tx.clone()
        )
        .await
        .err()
        .unwrap()
        .to_string()
        .contains("coordinator"));
    assert!(runner
        .run_turn(session, "bypass".into(), tx)
        .await
        .err()
        .unwrap()
        .to_string()
        .contains("teams/message"));
    let corrupt = reopened.teams.root.join(new_id());
    fs::create_dir_all(&corrupt).unwrap();
    fs::write(corrupt.join("team.json"), b"broken").unwrap();
    assert!(!reopened.teams.owns_session("ordinary-chat").unwrap());
    server.abort();
}

#[tokio::test]
async fn team_execution_lock_protects_live_native_runs_and_released_lock_recovers_retry() {
    let (fixture, mut requests, server) = fixture().await;
    let runtime = fixture.runtime.clone();
    let team = runtime
        .teams
        .create(
            &runtime,
            TeamSpec {
                name: "recovery".into(),
                goal: "retry the same worker".into(),
                agents: vec![worker("a", &[])],
                files: vec![],
            },
        )
        .unwrap();
    let prompt = agent_prompt(&team, &team.agents[0]);
    let task = runtime
        .teams
        .prepare_task(&runtime, &team, &team.agents[0], &prompt)
        .unwrap();
    runtime
        .tasks
        .transition_task(&task.id, TaskState::Running)
        .unwrap();
    let run = runtime.tasks.create_run(&task.id, 1).unwrap();
    runtime
        .tasks
        .claim_run(&run.id, "other-live-process", Utc::now())
        .unwrap();
    runtime
        .teams
        .mutate(&team.id, |team| {
            team.status = "running".into();
            team.agents[0].status = "running".into();
            team.agents[0].run_id = Some(run.id.clone());
            Ok(())
        })
        .unwrap();
    let lease = runtime.teams.run_lock(&team.id).unwrap();
    let reopened = Runtime::bootstrap(
        runtime.config.clone(),
        runtime.workspace_root.clone(),
        crate::DeepSeekClient::new(runtime.llm.config()).unwrap(),
        runtime.tools.clone(),
    )
    .unwrap();
    assert_eq!(reopened.teams.get(&team.id).unwrap().status, "running");
    assert_eq!(
        reopened.tasks.task(&task.id).unwrap().state,
        TaskState::Running
    );
    assert_eq!(
        reopened.tasks.run(&run.id).unwrap().state,
        RunState::Running
    );
    reopened.scheduler.tick_once().await;
    assert_eq!(
        reopened.tasks.run(&run.id).unwrap().state,
        RunState::Running
    );
    assert!(requests.try_recv().is_err());
    drop(lease);
    let interrupted = reopened.teams.get(&team.id).unwrap();
    assert_eq!(interrupted.status, "failed");
    assert_eq!(interrupted.agents[0].status, "failed");
    assert_eq!(
        reopened.tasks.task(&task.id).unwrap().state,
        TaskState::Failed
    );
    assert_eq!(reopened.tasks.run(&run.id).unwrap().state, RunState::Failed);
    reopened.teams.start(reopened.clone(), &team.id).unwrap();
    request(&mut requests)
        .await
        .reply
        .send("retried same native worker".into())
        .unwrap();
    let completed = settled(&reopened, &team.id).await;
    assert_eq!(completed.status, "completed");
    assert_eq!(completed.agents[0].subagent_id, team.agents[0].subagent_id);
    assert_eq!(completed.agents[0].session_id, team.agents[0].session_id);
    assert_eq!(
        completed.agents[0].task_id.as_deref(),
        Some(task.id.as_str())
    );
    assert_ne!(completed.agents[0].run_id.as_deref(), Some(run.id.as_str()));
    assert_eq!(
        reopened
            .tasks
            .latest_run_for_task(&task.id)
            .unwrap()
            .attempt,
        2
    );
    server.abort();
}
