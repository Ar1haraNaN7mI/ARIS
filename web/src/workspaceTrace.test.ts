import { describe, expect, it } from "vitest";
import { newTraceNodes, projectWorkspaceTrace } from "./workspaceTrace";
import type { Team, TeamAgent } from "./teamTypes";
import type { SessionEvent, SessionResult } from "./types";

const at = (second: number) => new Date(Date.UTC(2026, 10, 19, 0, 0, second)).toISOString();
const agent = (id: string, depends_on: string[] = []): TeamAgent => ({ id, depends_on, name: id, role: "research", profile_id: "gpt", session_id: `session-${id}`, prompt: "real task", files: [], status: "pending" });
const team = (id = "one"): Team => ({ id, name: id, goal: "Research actual sources", status: "running", agents: [agent("a"), agent("b", ["a"])], files: [], handoffs: [], checkpoints: [], events: [], created_at: at(0) });
const session = (id: string, events: SessionEvent[]): SessionResult => ({ session: { id, name: null, events }, task_id: null, state: "running" });
const event = (id: string, type: string, second: number, rest: Partial<SessionEvent> = {}): SessionEvent => ({ id, type, at: at(second), ...rest });

describe("real workspace trajectory", () => {
  it("does not invent execution events, hashes, models, or test counts for an unstarted team", () => {
    const value = team(); value.status = "idle";
    const [lane] = projectWorkspaceTrace([value]);
    expect(lane.nodes).toEqual([]);
    expect(lane.completed).toBe(0); expect(lane.total).toBe(2);
    expect(JSON.stringify(lane)).not.toMatch(/48\/48|hash|fake-model/);
  });

  it("uses detailed sessions only for the active task and checks the exact session identity", () => {
    const first = team(), second = team("two");
    first.events = second.events = [{ at: at(1), agent_id: "a", kind: "tool", message: "read_file" }];
    const sessions = { a: session("session-a", [event("reply", "assistant_message", 2, { text: "Real answer" })]) };
    const lanes = projectWorkspaceTrace([first, second], [], first.id, sessions);
    expect(lanes[0].nodes.map((node) => node.source)).toEqual(["session"]);
    expect(lanes[1].nodes.map((node) => node.source)).toEqual(["team"]);
    const wrong = projectWorkspaceTrace([first], [], first.id, { a: session("wrong-session", sessions.a.session.events) });
    expect(wrong[0].nodes.map((node) => node.source)).toEqual(["team"]);
  });

  it("pairs actual tool results per turn and never translates tool success into a fabricated test pass", () => {
    const value = team(); value.agents[0].status = "running";
    const events = [
      event("start-1", "turn_start", 1), event("call-1", "tool_call", 2, { call_id: "reused", name: "run_command", arguments: { command: "cargo test" } }),
      event("result-1", "tool_result", 3, { call_id: "reused", name: "run_command", content: "exit code 1", ok: true }), event("end-1", "turn_end", 4),
      event("start-2", "turn_start", 5), event("call-2", "tool_call", 6, { call_id: "reused", name: "read_file" }),
      event("result-2", "tool_result", 7, { call_id: "reused", name: "read_file", content: "file missing", ok: false }),
    ];
    const [lane] = projectWorkspaceTrace([value], [], value.id, { a: session("session-a", events) });
    expect(lane.nodes).toHaveLength(2);
    expect(lane.nodes.map((node) => [node.result, node.eventId, node.kind])).toEqual([["exit code 1", "result-1", "tool"], ["file missing", "result-2", "tool"]]);
    expect(lane.nodes.map((node) => node.anchorEventId)).toEqual(["call-1", "call-2"]);
    expect(lane.nodes[0].label).toBe("run_command");
    expect(lane.nodes[1].ok).toBe(false);
  });

  it("draws real independent branches and connects both predecessors to a merge", () => {
    const value = team(); value.agents = [agent("a"), agent("b", ["a"]), agent("c", ["a"]), agent("d", ["b", "c"])];
    value.events = [
      { at: at(1), agent_id: "a", kind: "agent_started", message: "a" },
      { at: at(2), agent_id: "a", kind: "agent_finished", message: "a: completed" },
      { at: at(3), agent_id: "b", kind: "agent_started", message: "b" },
      { at: at(4), agent_id: "c", kind: "agent_started", message: "c" },
      { at: at(5), agent_id: "b", kind: "agent_finished", message: "b: completed" },
      { at: at(6), agent_id: "c", kind: "agent_finished", message: "c: completed" },
      { at: at(7), agent_id: "d", kind: "agent_started", message: "d" },
    ];
    const [lane] = projectWorkspaceTrace([value]);
    expect(lane.nodes[2].branch).not.toBe(lane.nodes[3].branch);
    expect(lane.nodes[6].kind).toBe("merge");
    expect(lane.nodes[6].parentIds).toEqual([lane.nodes[4].id, lane.nodes[5].id]);
  });

  it("emits only new server events and does not replay first-load or expanded history", () => {
    const previous = new Map(); const value = team();
    value.events = [{ at: at(10), agent_id: "a", kind: "agent_started", message: "a" }];
    expect(newTraceNodes(projectWorkspaceTrace([value]), previous)).toEqual([]);
    value.events.push({ at: at(12), agent_id: "a", kind: "tool", message: "web_crawl" });
    expect(newTraceNodes(projectWorkspaceTrace([value]), previous).map((node) => node.label)).toEqual(["web_crawl"]);
    const expanded = { a: session("session-a", [event("old", "assistant_message", 5, { text: "Old reply" })]) };
    expect(newTraceNodes(projectWorkspaceTrace([value], [], value.id, expanded), previous)).toEqual([]);
    expanded.a.session.events.push(event("new", "assistant_message", 14, { text: "New reply" }));
    expect(newTraceNodes(projectWorkspaceTrace([value], [], value.id, expanded), previous).map((node) => node.eventId)).toEqual(["new"]);
  });

  it("accepts new session records arriving after a newer team poll without replay on selection", () => {
    const previous = new Map(); const value = team();
    value.events = [{ at: at(10), agent_id: "a", kind: "agent_started", message: "a" }];
    newTraceNodes(projectWorkspaceTrace([value]), previous);
    value.events.push({ at: at(15), agent_id: "a", kind: "agent_finished", message: "a: completed" });
    newTraceNodes(projectWorkspaceTrace([value]), previous);
    const sessions = { a: session("session-a", [event("old", "assistant_message", 5, { text: "Historical" }), event("late-poll", "assistant_message", 14, { text: "New actual reply" })]) };
    expect(newTraceNodes(projectWorkspaceTrace([value], [], value.id, sessions), previous).map((node) => node.eventId)).toEqual(["late-poll"]);
    newTraceNodes(projectWorkspaceTrace([value]), previous);
    expect(newTraceNodes(projectWorkspaceTrace([value], [], value.id, sessions), previous)).toEqual([]);
  });

  it("freezes each historical node to its turn identity and leaves legacy records unknown", () => {
    const value = team(); value.agents[0].profile_id = "current-kimi";
    const events = [
      event("turn-a", "turn_start", 1, { profile_id: "original-gpt", model_id: "gpt-recorded", family: "gpt" }),
      event("reply-a", "assistant_message", 2, { text: "Original answer" }), event("end-a", "turn_end", 3),
      event("turn-b", "turn_start", 4, { profile_id: "original-gemini", model_id: "gemini-recorded", family: "gemini" }),
      event("call-b", "tool_call", 5, { call_id: "real-call", name: "read_file" }),
      event("result-b", "tool_result", 6, { call_id: "real-call", name: "read_file", ok: true, content: "real text" }), event("end-b", "turn_end", 7),
      event("legacy-turn", "turn_start", 8), event("legacy-reply", "assistant_message", 9, { text: "Legacy answer" }),
    ];
    const [lane] = projectWorkspaceTrace([value], [], value.id, { a: session("session-a", events) });
    expect(lane.nodes.map((node) => node.identity?.modelId)).toEqual(["gpt-recorded", "gemini-recorded", undefined]);
    expect(lane.nodes.map((node) => node.identity?.family)).toEqual(["gpt", "gemini", undefined]);
  });
});
