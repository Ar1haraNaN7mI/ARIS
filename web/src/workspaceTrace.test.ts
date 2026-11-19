import { describe, expect, it } from "vitest";
import { newTraceNodes, projectWorkspaceTrace, type TraceLane, type WorkspaceTrace } from "./workspaceTrace";
import type { Team, TeamAgent } from "./teamTypes";
import type { SessionEvent, SessionResult } from "./types";
const at = (second: number) => new Date(Date.UTC(2026, 10, 19) + second * 1000).toISOString();
const agent = (id: string, depends_on: string[] = []): TeamAgent => ({ id, depends_on, name: id, role: "research", profile_id: "gpt", session_id: `session-${id}`, prompt: "real task", files: [], status: "pending" });
const team = (id = "one"): Team => ({ id, name: id, goal: "Research actual sources", status: "running", agents: [agent("a"), agent("b", ["a"])], files: [], handoffs: [], checkpoints: [], events: [], created_at: at(0) });
const session = (id: string, events: SessionEvent[]): SessionResult => ({ session: { id, name: null, events }, task_id: null, state: "running" });
const event = (id: string, type: string, second: number, rest: Partial<SessionEvent> = {}): SessionEvent => ({ id, type, at: at(second), ...rest });
const role = (projection: WorkspaceTrace, id: string, teamId = "one") => projection.lanes.find((lane) => lane.agent.id === id && lane.team.id === teamId)!;
const records = (projection: WorkspaceTrace) => [...projection.lanes.flatMap((lane) => lane.nodes), ...[...projection.teamEvents.values()].flat()];
const staged = (): Team => ({ ...team(), agents: [agent("a"), agent("b", ["a"]), agent("c", ["a"]), agent("d", ["b", "c"])], events: [
  { at: at(1), agent_id: "a", kind: "agent_started", message: "a" },
  { at: at(2), agent_id: "a", kind: "agent_finished", message: "a: completed" },
  { at: at(3), agent_id: "b", kind: "agent_started", message: "b" },
  { at: at(4), agent_id: "c", kind: "agent_started", message: "c" },
  { at: at(5), agent_id: "b", kind: "agent_finished", message: "b: completed" },
  { at: at(6), agent_id: "c", kind: "agent_finished", message: "c: completed" },
  { at: at(7), agent_id: "d", kind: "agent_started", message: "d" },
] });

describe("one real role per trajectory lane", () => {
  it("creates separate same-model roles without inventing execution records", () => {
    const value = team(); value.status = "idle"; value.agents.push(agent("third"));
    const result = projectWorkspaceTrace([value]);
    expect(result.lanes.map((lane) => lane.agent.id)).toEqual(["a", "b", "third"]);
    expect(new Set(result.lanes.map((lane) => lane.id)).size).toBe(3);
    expect(new Set(result.lanes.map((lane) => lane.color)).size).toBe(3);
    expect(result.lanes.every((lane) => lane.nodes.length === 0 && lane.width === 124)).toBe(true);
    expect(result.teamEvents.get(value.id)).toEqual([]);
    expect(JSON.stringify(result.lanes)).not.toMatch(/48\/48|fake-model/);
  });
  it("uses detailed sessions only for the active team and exact persisted session", () => {
    const first = team(), second = team("two");
    first.events = second.events = [{ at: at(1), agent_id: "a", kind: "tool", message: "read_file" }];
    const sessions = { a: session("session-a", [event("reply", "assistant_message", 2, { text: "Real answer" })]) };
    const result = projectWorkspaceTrace([first, second], [], first.id, sessions);
    expect(role(result, "a").nodes.filter((node) => node.source === "session")).toHaveLength(1);
    expect(role(result, "a", "two").nodes.map((node) => node.source)).toEqual(["team"]);
    expect(role(projectWorkspaceTrace([first], [], first.id, { a: session("wrong", sessions.a.session.events) }), "a").nodes.map((node) => node.source)).toEqual(["team"]);
  });
  it("namespaces repeated role and event IDs across teams without cross-team edges", () => {
    const first = staged(), second = { ...staged(), id: "two" };
    const result = projectWorkspaceTrace([first, second]);
    const nodes = records(result); const byId = new Map(nodes.map((node) => [node.id, node]));
    expect(new Set(nodes.map((node) => node.id)).size).toBe(nodes.length);
    expect(new Set(result.lanes.map((lane) => lane.id)).size).toBe(8);
    for (const node of nodes) for (const id of node.parentIds) expect(byId.get(id)?.teamId).toBe(node.teamId);
  });
  it("pairs tool records by turn/call identity and does not invent test results", () => {
    const value = team(); value.agents[0].status = "running";
    const events = [event("turn1", "turn_start", 1), event("call1", "tool_call", 2, { call_id: "reused", name: "run_command", arguments: { command: "cargo test" } }),
      event("result1", "tool_result", 3, { call_id: "reused", name: "run_command", content: "exit code 1", ok: true }), event("turn1", "turn_end", 4),
      event("turn2", "turn_start", 5), event("call2", "tool_call", 6, { call_id: "reused", name: "read_file" }), event("result2", "tool_result", 7, { call_id: "reused", name: "read_file", content: "missing", ok: false })];
    const lane = role(projectWorkspaceTrace([value], [], value.id, { a: session("session-a", events) }), "a");
    expect(lane.nodes.map((node) => [node.result, node.eventId, node.kind])).toEqual([["exit code 1", "result1", "tool"], ["missing", "result2", "tool"]]);
    expect(lane.nodes.map((node) => node.anchorEventId)).toEqual(["call1", "call2"]);
    expect(lane.nodes[0].label).toBe("run_command");
    expect(lane.nodes[1].ok).toBe(false);
  });
  it("keeps newer tool summaries live and moves the covered prefix to inspectable records", () => {
    const value = team();
    value.events = [{ at: at(1), agent_id: "a", kind: "tool", message: "read_file" }, { at: at(2), agent_id: "a", kind: "tool_finished", message: "read_file: completed" }, { at: at(4), agent_id: "a", kind: "tool", message: "new_call" }];
    const events = [event("call", "tool_call", 1, { call_id: "c", name: "read_file" }), event("result", "tool_result", 2.1, { call_id: "c", name: "read_file", ok: true, content: "Actual content" })];
    const result = projectWorkspaceTrace([value], [], value.id, { a: session("session-a", events) });
    expect(role(result, "a").nodes.map((node) => node.label)).toEqual(["read_file", "new_call"]);
    expect(result.teamEvents.get(value.id)?.map((node) => [node.agentId, node.teamEventKind])).toEqual([["a", "tool"], ["a", "tool_finished"]]);
    const precise = { ...value, events: [{ at: "2026-11-19T00:00:02.000200Z", agent_id: "a", kind: "tool_finished", message: "read_file: completed" }] };
    const earlier = [events[0], { ...events[1], at: "2026-11-19T00:00:02.000100Z" }];
    expect(role(projectWorkspaceTrace([precise], [], precise.id, { a: session("session-a", earlier) }), "a").nodes).toHaveLength(2);
  });
  it("lays independent roles side by side and merges actual predecessor deliveries", () => {
    const result = projectWorkspaceTrace([staged()]);
    const b = role(result, "b"), c = role(result, "c"), d = role(result, "d");
    expect(b.nodes[0].y).toBe(c.nodes[0].y);
    expect(d.nodes[0].kind).toBe("merge");
    expect(d.nodes[0].parentIds).toEqual([b.nodes[1].id, c.nodes[1].id]);
    expect(Object.values(d.nodes[0].parentKinds)).toEqual(["dependency", "dependency"]);
    expect(d.nodes[0].y).toBeGreaterThan(c.nodes[1].y);
    expect(d.nodes[0].laneId).toBe(d.id);
  });
  it("continues one role without drawing old inputs as a second merge", () => {
    const value = staged(); value.events.push({ at: at(8), agent_id: "d", kind: "agent_finished", message: "d: completed" }, { at: at(9), agent_id: "d", kind: "agent_started", message: "d" });
    const d = role(projectWorkspaceTrace([value]), "d");
    expect(d.nodes[2].kind).toBe("start");
    expect(d.nodes[2].parentIds).toEqual([d.nodes[1].id]);
    expect(Object.values(d.nodes[2].parentKinds)).toEqual(["sequence"]);
  });
  it("only reconnects a dependency when that predecessor has a new delivery", () => {
    const value = staged(); value.events.push({ at: at(8), agent_id: "d", kind: "agent_finished", message: "d: completed" }, { at: at(9), agent_id: "b", kind: "agent_started", message: "b" }, { at: at(10), agent_id: "b", kind: "agent_finished", message: "b: completed" }, { at: at(11), agent_id: "d", kind: "agent_started", message: "d" });
    const result = projectWorkspaceTrace([value]), d = role(result, "d"), b = role(result, "b");
    expect(d.nodes[2].parentIds).toEqual([d.nodes[1].id, b.nodes[3].id]);
    expect(Object.values(d.nodes[2].parentKinds)).toEqual(["sequence", "dependency"]);
  });
  it("represents explicit handoffs separately from declared dependencies", () => {
    const value = team(); value.events = [{ at: at(2), agent_id: "a", kind: "agent_finished", message: "a: completed" }, { at: at(3.01), agent_id: "b", kind: "handoff", message: "a → b: Verify this" }];
    value.handoffs = [{ id: "h", from: "a", to: "b", instruction: "Verify this", summary: "Actual source snapshot", at: at(3) }];
    const result = projectWorkspaceTrace([value]), node = role(result, "b").nodes[0];
    expect(node.source).toBe("handoff"); expect(node.detail).toContain("Actual source snapshot");
    expect(node.parentIds).toEqual([role(result, "a").nodes[0].id]);
    expect(Object.values(node.parentKinds)).toContain("handoff");
    expect(result.teamEvents.get(value.id)?.some((record) => record.teamEventKind === "handoff")).toBe(true);
  });
  it("puts only explicitly owned admin events in its real management conversation", () => {
    const value = team(); value.agents.push({ ...agent("host"), name: "调度管理员" });
    value.admin = { session_id: "session-host", profile_id: "gpt", model_id: "actual-admin", family: "gpt", phase: "answered", response: "Current answer" };
    value.events = [{ at: at(0), kind: "created", message: "created" }, { at: at(1), kind: "admin_started", message: "starting" }, { at: at(2), kind: "admin_stopped", message: "Old outcome not classified" }, { at: at(3), kind: "finished", message: "All agents completed" }, { at: at(4), agent_id: "missing-role", kind: "agent_error", message: "Unavailable role" }];
    const result = projectWorkspaceTrace([value]), host = role(result, "host");
    expect(host.kind).toBe("admin"); expect(host.agent.name).toBe("调度管理员");
    expect(host.nodes.map((node) => node.teamEventKind)).toEqual(["admin_started", "admin_stopped"]);
    expect(host.nodes[1].kind).toBe("note");
    expect(result.teamEvents.get(value.id)?.map((node) => node.teamEventKind)).toEqual(["created", "finished", "agent_error"]);
    expect(result.lanes).toHaveLength(3);
  });
  it("shows actual admin user text and identifies worker input as execution context", () => {
    const value = team(); value.admin = { session_id: "admin-session", profile_id: "gpt", model_id: "model", family: "gpt", phase: "answered", response: "" };
    const initial = projectWorkspaceTrace([value]); const host = initial.lanes.find((lane) => lane.kind === "admin")!;
    const result = projectWorkspaceTrace([value], [], value.id, { [host.agent.id]: session("admin-session", [event("u", "user_message", 2, { text: "Actual follow-up question" })]), a: session("session-a", [event("ctx", "user_message", 2, { text: "You are a in ARIS. Internal execution context." })]) });
    expect(role(result, host.agent.id).nodes[0].label).toBe("Actual follow-up question");
    expect(role(result, "a").nodes[0]).toMatchObject({ kind: "note", label: "接收执行上下文", detail: "You are a in ARIS. Internal execution context." });
    value.agents.push(agent("later"));
    expect(projectWorkspaceTrace([value]).lanes.find((lane) => lane.kind === "admin")?.color).toBe(host.color);
  });
  it("coalesces each checkpoint summary by durable ID, retaining files and repeated labels", () => {
    const value = team(); value.events = [{ at: at(2.001), kind: "checkpoint", message: "Same label" }, { at: at(3.001), kind: "checkpoint", message: "Same label" }, { at: at(4), kind: "restored", message: "Restored files" }];
    value.checkpoints = [{ id: "c1", at: at(2), label: "Same label", files: [{ path: "a.md", exists: true, size: 12 }] }, { id: "c2", at: at(3), label: "Same label", files: [{ path: "b.md", exists: false, size: 0 }] }];
    const result = projectWorkspaceTrace([value]); const saved = result.teamEvents.get(value.id)!;
    expect(saved).toHaveLength(3); expect(saved.filter((node) => node.checkpointId).map((node) => node.checkpointId)).toEqual(["c1", "c2"]);
    expect(saved[0].files).toEqual(value.checkpoints[0].files); expect(saved[0].summaryEvent?.message).toBe("Same label");
    expect(saved[2].label).toBe("已恢复登记文件"); expect(result.lanes.every((lane) => lane.nodes.length === 0)).toBe(true);
  });
  it("freezes historical model identities instead of using the current profile", () => {
    const value = team(); value.agents[0].profile_id = "current-kimi";
    const events = [event("turn1", "turn_start", 1, { model_id: "gpt-recorded", family: "gpt" }), event("reply1", "assistant_message", 2, { text: "First" }), event("turn1", "turn_end", 3), event("turn2", "turn_start", 4, { model_id: "gemini-recorded", family: "gemini" }), event("reply2", "assistant_message", 5, { text: "Second" }), event("legacy", "turn_start", 6), event("reply3", "assistant_message", 7, { text: "Unknown" })];
    expect(role(projectWorkspaceTrace([value], [], value.id, { a: session("session-a", events) }), "a").nodes.map((node) => node.identity?.modelId)).toEqual(["gpt-recorded", "gemini-recorded", undefined]);
  });
});

describe("role-local real-event animation history", () => {
  it("does not replay initial or expanded history and accepts delayed new session records", () => {
    const previous = new Map(); const value = team(); value.events = [{ at: at(10), agent_id: "a", kind: "agent_started", message: "a" }];
    expect(newTraceNodes(projectWorkspaceTrace([value]).lanes, previous)).toEqual([]);
    value.events.push({ at: at(15), agent_id: "a", kind: "agent_finished", message: "a: completed" });
    expect(newTraceNodes(projectWorkspaceTrace([value]).lanes, previous)).toHaveLength(1);
    const sessions = { a: session("session-a", [event("old", "assistant_message", 5, { text: "Old" }), event("late", "assistant_message", 14, { text: "New" })]) };
    expect(newTraceNodes(projectWorkspaceTrace([value], [], value.id, sessions).lanes, previous).map((node) => node.eventId)).toEqual(["late"]);
    newTraceNodes(projectWorkspaceTrace([value]).lanes, previous);
    expect(newTraceNodes(projectWorkspaceTrace([value], [], value.id, sessions).lanes, previous)).toEqual([]);
  });
  it("does not replay hidden lanes when returning from current-team filtering", () => {
    const previous = new Map(); const a = team(), b = team("two");
    a.events = [{ at: at(1), agent_id: "a", kind: "agent_started", message: "a" }]; b.events = a.events.map((entry) => ({ ...entry }));
    newTraceNodes(projectWorkspaceTrace([a, b]).lanes, previous);
    newTraceNodes(projectWorkspaceTrace([a]).lanes, previous);
    b.events.push({ at: at(2), agent_id: "a", kind: "tool", message: "hidden" });
    expect(newTraceNodes(projectWorkspaceTrace([a, b]).lanes, previous)).toEqual([]);
    b.events.push({ at: at(3), agent_id: "a", kind: "tool", message: "visible" });
    expect(newTraceNodes(projectWorkspaceTrace([a, b]).lanes, previous).map((node) => node.label)).toEqual(["visible"]);
  });
  it("emits the first actual events of a newly created role after the initial baseline", () => {
    const previous = new Map(); const value = team(); value.agents = [agent("a")]; value.events = [{ at: at(1), agent_id: "a", kind: "agent_started", message: "a" }];
    newTraceNodes(projectWorkspaceTrace([value]).lanes, previous);
    value.agents.push(agent("new-role")); value.events.push({ at: at(2), agent_id: "new-role", kind: "agent_started", message: "new-role" });
    expect(newTraceNodes(projectWorkspaceTrace([value]).lanes, previous).map((node) => node.agentId)).toEqual(["new-role"]);
  });
  it("accepts a large history without argument-spread limits", () => {
    const previous = new Map(); const lane = projectWorkspaceTrace([team()]).lanes[0];
    lane.nodes = Array.from({ length: 150_000 }, (_, i) => ({ id: `long-${i}`, revision: `long-${i}`, teamId: lane.team.id, laneId: lane.id, agentId: lane.agent.id, at: at(i), kind: "note" as const, label: "Actual record", detail: "", source: "session" as const, parentIds: [], parentKinds: {}, y: i }));
    expect(newTraceNodes([lane] as TraceLane[], previous)).toEqual([]);
    expect(previous.get(lane.id).revisions.size).toBe(150_000);
  });
});
