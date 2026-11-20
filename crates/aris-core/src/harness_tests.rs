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
                    let payload = text.strip_prefix("LOCAL_TOOL_FIXTURE:").map(str::to_owned).unwrap_or_else(||
                        json!({"choices":[{"message":{"role":"assistant","content":text},"finish_reason":"stop"}]}).to_string());
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

fn tool_reply(request: Request, call_id: &str, args: Value) {
    worker_tool_reply(request, call_id, "agent_tunnel", args);
}

fn worker_tool_reply(request: Request, call_id: &str, name: &str, args: Value) {
    let body = json!({"choices":[{"message":{"role":"assistant","content":"", "tool_calls":[{
        "id":call_id,"type":"function","function":{"name":name,"arguments":args.to_string()}
    }]},"finish_reason":"tool_calls"}]});
    request
        .reply
        .send(format!("LOCAL_TOOL_FIXTURE:{body}"))
        .unwrap();
}

async fn real_tunnel_roundtrip(dependent: bool) {
    let (fixture, mut rx, server) = fixture().await;
    let runtime = fixture.runtime.clone();
    let mut final_agent = worker("c", &["a", "b"]);
    final_agent.delivery = Some(crate::synthesis::DeliveryKind::Final);
    let team = runtime
        .teams
        .create(
            &runtime,
            TeamSpec {
                name: "real peer tools".into(),
                goal: "Consult the peer then deliver".into(),
                agents: vec![
                    worker("a", &[]),
                    worker("b", if dependent { &["a"] } else { &[] }),
                    final_agent,
                ],
                files: vec![],
            },
        )
        .unwrap();
    let a_session = team.agents[0].session_id.clone();
    runtime.teams.start(runtime.clone(), &team.id).unwrap();
    let first = request(&mut rx).await;
    let (a, held_b) = if dependent {
        (first, None)
    } else {
        let second = request(&mut rx).await;
        if first
            .body
            .to_string()
            .contains("Your assignment: Answer as a")
        {
            (first, Some(second))
        } else {
            (second, Some(first))
        }
    };
    assert!(a.body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["function"]["name"] == "agent_tunnel"));
    tool_reply(
        a,
        "open-peer",
        json!({"action":"ask","to":"b","topic":"Verify the conclusion","text":"Please verify the denominator before I finalize.","resume_on_reply":true}),
    );
    let a_after_open = request(&mut rx).await;
    assert!(a_after_open
        .body
        .to_string()
        .contains("Please verify the denominator"));
    a_after_open
        .reply
        .send("Initial analysis; peer consultation requested.".into())
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while runtime.teams.get(&team.id).unwrap().agents[0].status != "completed" {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let initial_a_task = runtime.teams.get(&team.id).unwrap().agents[0]
        .task_id
        .clone();
    let b = if let Some(held_b) = held_b {
        held_b
            .reply
            .send("Draft before the pending peer message was delivered.".into())
            .unwrap();
        request(&mut rx).await
    } else {
        request(&mut rx).await
    };
    assert!(b.body.to_string().contains("Please verify the denominator"));
    let open = runtime.teams.get(&team.id).unwrap().tunnels[0].clone();
    tool_reply(
        b,
        "reply-peer",
        json!({"action":"send","tunnel_id":open.id,"reply_to":open.messages[0].id,
        "text":"Confirmed: the denominator is 25 trial applications, not 8 activations.","expects_reply":false}),
    );
    // The subscribed result can reach A while B is still finishing its turn.
    let first = request(&mut rx).await;
    let second = request(&mut rx).await;
    let (resumed_a, b_after_answer) = if first
        .body
        .to_string()
        .contains("Your assignment: Answer as a")
    {
        (first, second)
    } else {
        (second, first)
    };
    b_after_answer
        .reply
        .send("Peer verification complete.".into())
        .unwrap();
    assert!(resumed_a
        .body
        .to_string()
        .contains("Confirmed: the denominator is 25"));
    assert!(resumed_a
        .body
        .to_string()
        .contains("Your assignment: Answer as a"));
    resumed_a
        .reply
        .send("Final analysis corrected using the peer's denominator.".into())
        .unwrap();
    let final_request = request(&mut rx).await;
    assert!(final_request
        .body
        .to_string()
        .contains("Final analysis corrected"));
    final_request
        .reply
        .send("Merged final deliverable with the verified denominator.".into())
        .unwrap();
    let completed = settled(&runtime, &team.id).await;
    assert_eq!(completed.status, "completed");
    let delivery = crate::synthesis::deliverable(&runtime, &completed)
        .unwrap()
        .unwrap();
    assert!(delivery.text.contains("Merged final deliverable"));
    assert!(
        !delivery.stale,
        "Final evidence must include the completed peer exchange"
    );
    assert_eq!(completed.tunnels.len(), 1);
    let tunnel = &completed.tunnels[0];
    assert_eq!(tunnel.status, "closed");
    assert!(
        tunnel.decision.is_none(),
        "a question and answer is not mutual acceptance"
    );
    assert_eq!(tunnel.messages.len(), 2);
    assert_eq!(tunnel.messages[0].from, "a");
    assert_eq!(tunnel.messages[1].from, "b");
    assert!(tunnel
        .messages
        .iter()
        .all(|message| message.delivered_at.is_some()));
    assert_eq!(completed.tunnel_turns.get("a"), Some(&1));
    assert_eq!(completed.agents[0].session_id, a_session);
    assert_ne!(completed.agents[0].task_id, initial_a_task);
    for agent in &completed.agents[..2] {
        let session = runtime.sessions.get_or_load(&agent.session_id).unwrap();
        let session = session.read();
        assert!(session.events.iter().any(
            |event| matches!(event,crate::SessionEvent::ToolCall{name,..} if name == "agent_tunnel")
        ));
        assert!(session.events.iter().any(|event|matches!(event,crate::SessionEvent::ToolResult{name,ok:true,..} if name == "agent_tunnel")));
        assert!(session.events.iter().any(|event|matches!(event,crate::SessionEvent::SystemNote{text,..} if text.starts_with("Peer coordination"))));
    }
    assert!(rx.try_recv().is_err());
    server.abort();
}

#[tokio::test]
async fn native_tunnel_parallel_peer_and_late_reply_resume_original_worker_once() {
    real_tunnel_roundtrip(false).await;
}

#[tokio::test]
async fn native_tunnel_delivers_to_a_dependency_that_has_not_started() {
    real_tunnel_roundtrip(true).await;
}

#[tokio::test]
async fn cowork_runs_in_the_invited_lane_and_adoption_finishes_without_an_acknowledgment() {
    let (fixture, mut requests, server) = fixture().await;
    let runtime = fixture.runtime.clone();
    runtime
        .tools
        .register(Arc::new(crate::report_calculate::ReportCalculate));
    let mut writer = worker("writer", &["finance", "review"]);
    writer.delivery = Some(crate::synthesis::DeliveryKind::Final);
    let team = runtime
        .teams
        .create(
            &runtime,
            TeamSpec {
                name: "Release review".into(),
                goal: "Review the budget and publish the decision".into(),
                agents: vec![
                    worker("finance", &[]),
                    worker("review", &["finance"]),
                    writer,
                ],
                files: vec![],
            },
        )
        .unwrap();
    runtime.teams.start(runtime.clone(), &team.id).unwrap();
    request(&mut requests)
        .await
        .reply
        .send("Base budget checked.".into())
        .unwrap();
    tool_reply(
        request(&mut requests).await,
        "invite",
        json!({
            "action":"invite","to":"finance","topic":"Cash reserve","text":"Calculate the reserve for six seats at 299 each."
        }),
    );
    // Keep the inviter's actual HTTP request open. With the old wave barrier
    // the second request would never arrive until the first one finished.
    let first = request(&mut requests).await;
    let second = request(&mut requests).await;
    let (helper, held_inviter) = if first
        .body
        .to_string()
        .contains("Your assignment: Answer as finance")
    {
        (first, second)
    } else {
        (second, first)
    };
    let saved = runtime.teams.get(&team.id).unwrap();
    let tunnel = &saved.tunnels[0];
    let invite = &tunnel.messages[0].id;
    assert_eq!(
        tunnel.messages[0].recipient_status_at_send.as_deref(),
        Some("completed")
    );
    let started = tunnel.messages[0].started_at.as_ref().unwrap();
    assert!(saved.events.iter().any(|e| e.kind == "agent_started"
        && e.agent_id.as_deref() == Some("finance")
        && &e.at == started));
    assert!(
        saved
            .agents
            .iter()
            .filter(|a| a.status == "running")
            .count()
            == 2
    );
    assert!(helper.body["messages"]
        .to_string()
        .contains("Calculate the reserve"));
    tool_reply(
        helper,
        "take-work",
        json!({"action":"accept_work","tunnel_id":tunnel.id,"reply_to":invite}),
    );
    worker_tool_reply(
        request(&mut requests).await,
        "calculate",
        "report_calculate",
        json!({
            "checks":[{"label":"Refund reserve","terms":[[6,299]]}]
        }),
    );
    let calculated = request(&mut requests).await;
    assert!(calculated.body["messages"].to_string().contains("1794"));
    tool_reply(
        calculated,
        "return-work",
        json!({
            "action":"complete_work","tunnel_id":tunnel.id,"reply_to":invite,"text":"Six seats require a reserve of 1794."
        }),
    );
    request(&mut requests)
        .await
        .reply
        .send("Base budget checked. Refund reserve verified at 1794.".into())
        .unwrap();
    held_inviter
        .reply
        .send("Reviewed the launch conditions. Finance verified the reserve.".into())
        .unwrap();
    let publication = request(&mut requests).await;
    assert!(publication.body["messages"]
        .to_string()
        .contains("Six seats require a reserve of 1794."));
    tool_reply(
        publication,
        "adopted",
        json!({
            "action":"open","to":"finance","topic":"Final amount","text":"已采纳，报告已使用核验金额。"
        }),
    );
    request(&mut requests)
        .await
        .reply
        .send("Approve a refund reserve of 1794.".into())
        .unwrap();

    let finished = settled(&runtime, &team.id).await;
    assert_eq!(finished.status, "completed");
    assert!(finished
        .agents
        .iter()
        .all(|agent| agent.status == "completed"));
    assert_eq!(finished.tunnel_turns.len(), 1);
    assert_eq!(finished.tunnel_turns["finance"], 1);
    let messages = &finished.tunnels[0].messages;
    assert_eq!(
        messages
            .iter()
            .map(|msg| msg.kind.as_str())
            .collect::<Vec<_>>(),
        ["cowork_invite", "cowork_accept", "cowork_result"]
    );
    for receipt in &messages[1..] {
        assert_eq!(receipt.reply_to.as_ref(), Some(invite));
        assert_eq!(receipt.session_id, team.agents[0].session_id);
    }
    let notice_tunnel = &finished.tunnels[1];
    assert!(
        notice_tunnel.participants.contains(&"finance".into())
            && notice_tunnel.participants.contains(&"writer".into())
    );
    assert_eq!(notice_tunnel.messages[0].kind, "notice");
    assert!(notice_tunnel.messages[0].resumed_at.is_none());
    let delivery = crate::synthesis::deliverable(&runtime, &finished)
        .unwrap()
        .unwrap();
    assert_eq!(delivery.text, "Approve a refund reserve of 1794.");
    assert!(!delivery.stale);
    assert!(requests.try_recv().is_err());
    server.abort();
}

#[tokio::test]
async fn first_free_slot_serves_cowork_before_primary_dependencies_finish() {
    let (fixture, mut rx, server) = fixture().await;
    let runtime = fixture.runtime.clone();
    let mut writer = worker("editor", &["audit", "growth", "research"]);
    writer.delivery = Some(crate::synthesis::DeliveryKind::Final);
    let team = runtime
        .teams
        .create(
            &runtime,
            TeamSpec {
                name: "First available lane".into(),
                goal: "Verify and publish".into(),
                agents: vec![
                    worker("budget", &[]),
                    worker("growth", &[]),
                    worker("research", &[]),
                    worker("audit", &["budget"]),
                    writer,
                ],
                files: vec![],
            },
        )
        .unwrap();
    runtime.teams.mutate(&team.id, |team|{
        team.execution_budget = Some(serde_json::from_value(json!({
            "agent_ids":["budget","research","growth","audit","editor"],"max_agent_runs":10,"max_steps_per_agent":18,
            "max_tool_calls_per_agent":32,"max_context_chars":64000,"max_tokens_per_step":4096,"max_parallel":3,"max_runtime_secs":120
        }))?);
        Ok(())
    }).unwrap();
    runtime.teams.start(runtime.clone(), &team.id).unwrap();
    let mut held = HashMap::new();
    for _ in 0..3 {
        let req = request(&mut rx).await;
        let body = req.body.to_string();
        let role = ["budget", "growth", "research"]
            .into_iter()
            .find(|r| body.contains(&format!("Your assignment: Answer as {r}")))
            .unwrap();
        held.insert(role, req);
    }
    tool_reply(
        held.remove("budget").unwrap(),
        "invite-audit",
        json!({
            "action":"invite","to":"audit","topic":"Compute margin","text":"What is the contribution of 10 teams at 90 each?"
        }),
    );
    let waiting_budget = request(&mut rx).await;
    let state = runtime.teams.get(&team.id).unwrap();
    let tunnel = &state.tunnels[0];
    let invite = &tunnel.messages[0];
    assert_eq!(invite.recipient_status_at_send.as_deref(), Some("pending"));
    assert!(invite.started_at.is_none());
    assert_eq!(
        state
            .agents
            .iter()
            .filter(|a| a.status == "running")
            .count(),
        3
    );
    held.remove("growth")
        .unwrap()
        .reply
        .send("Channel experiment ready".into())
        .unwrap();
    let peer = request(&mut rx).await;
    assert!(peer.body["messages"]
        .to_string()
        .contains("separate, bounded co-work turn"));
    let active = runtime.teams.get(&team.id).unwrap();
    assert_eq!(
        active
            .agents
            .iter()
            .filter(|a| a.status == "running")
            .count(),
        3
    );
    assert_eq!(
        active.tunnels[0].messages[0].started_as.as_deref(),
        Some("co_work")
    );
    tool_reply(
        peer,
        "accept",
        json!({"action":"accept_work","tunnel_id":tunnel.id,"reply_to":invite.id}),
    );
    tool_reply(
        request(&mut rx).await,
        "result",
        json!({"action":"complete_work","tunnel_id":tunnel.id,"reply_to":invite.id,"text":"10 × 90 = 900."}),
    );
    request(&mut rx)
        .await
        .reply
        .send("Contribution verified.".into())
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while runtime.teams.get(&team.id).unwrap().agents[3].status != "pending" {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let pending = runtime.teams.get(&team.id).unwrap();
    assert_eq!(pending.agents[3].output, "");
    assert!(pending.agents[3].error.is_none());
    let first_task = pending.agents[3].task_id.clone();
    assert!(rx.try_recv().is_err());
    waiting_budget
        .reply
        .send("Budget assignment now finished.".into())
        .unwrap();
    let audit = request(&mut rx).await;
    assert!(audit.body["messages"]
        .to_string()
        .contains("Your assignment: Answer as audit"));
    audit
        .reply
        .send("Independent audit of the completed budget.".into())
        .unwrap();
    held.remove("research")
        .unwrap()
        .reply
        .send("Sources ready.".into())
        .unwrap();
    let editor = request(&mut rx).await;
    assert!(editor.body["messages"]
        .to_string()
        .contains("Independent audit of the completed budget."));
    editor.reply.send("Final verified report.".into()).unwrap();
    let end = settled(&runtime, &team.id).await;
    assert_eq!(end.status, "completed");
    assert_eq!(end.tunnel_turns["audit"], 1);
    assert_ne!(end.agents[3].task_id, first_task);
    assert_eq!(end.agents[3].session_id, team.agents[3].session_id);
    assert_eq!(
        end.events
            .iter()
            .filter(|e| e.kind == "cowork_turn_started")
            .count(),
        1
    );
    assert_eq!(
        end.events
            .iter()
            .filter(|e| e.kind == "agent_started" && e.agent_id.as_deref() == Some("audit"))
            .count(),
        2
    );
    assert!(
        !crate::synthesis::deliverable(&runtime, &end)
            .unwrap()
            .unwrap()
            .stale
    );
    server.abort();
}

#[tokio::test]
async fn worker_missing_an_assigned_document_gets_one_same_session_correction() {
    let (fixture, mut rx, server) = fixture().await;
    let runtime = fixture.runtime.clone();
    let mut researcher = worker("researcher", &[]);
    researcher.delivery = Some(crate::synthesis::DeliveryKind::Supporting);
    researcher.files = vec!["research.md".into()];
    let team = runtime
        .teams
        .create(
            &runtime,
            TeamSpec {
                name: "missing assigned report".into(),
                goal: "Write a sourced research report".into(),
                agents: vec![researcher],
                files: vec!["research.md".into()],
            },
        )
        .unwrap();
    runtime.teams.start(runtime.clone(), &team.id).unwrap();
    request(&mut rx)
        .await
        .reply
        .send("Which report would you like?".into())
        .unwrap();
    let correction = request(&mut rx).await;
    let last = correction.body["messages"]
        .as_array()
        .unwrap()
        .last()
        .unwrap();
    assert!(last["content"].as_str().unwrap().contains("research.md"));
    assert!(last["content"].as_str().unwrap().contains("verified write"));
    correction
        .reply
        .send("No document delivered".into())
        .unwrap();
    let end = settled(&runtime, &team.id).await;
    assert_eq!(end.status, "failed");
    assert!(end.agents[0]
        .error
        .as_deref()
        .unwrap()
        .contains("research.md"));
    assert!(
        rx.try_recv().is_err(),
        "Do not repeat an unsuccessful recovery"
    );
    server.abort();
}
