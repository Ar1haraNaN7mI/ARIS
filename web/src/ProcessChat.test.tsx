import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { conversationEntries, ProcessChat } from "./ProcessChat";
import { agentStages } from "./ProcessRelay";
import type { Team, TeamAgent } from "./teamTypes";
import type { SessionResult } from "./types";

const at = (second: number) => `2026-11-19T00:00:${String(second).padStart(2, "0")}Z`;
afterEach(cleanup);
const agent = (id: string, deps: string[] = []): TeamAgent => ({ id, name: id, role: id, profile_id: "gpt", prompt: "", files: [], depends_on: deps, status: "completed", session_id: `session-${id}` });
const makeTeam = (): Team => ({ id: "t", name: "Task", goal: "Initial request", created_at: at(0), status: "completed", agents: [agent("a"), agent("b")], files: [], events: [], handoffs: [], checkpoints: [], messages: [{ id: "m", text: "Follow up", at: at(2), queued: true }] });
describe("Process conversation uses real chronological records", () => {
  it("interleaves historical replies with user messages across agents", () => {
    const team = makeTeam();
    const sessions = Object.fromEntries(["a", "b"].map((id, index) => [id, { session: { id: `session-${id}`, name: id, events: [{ id: "reply", type: "assistant_message", at: at(index ? 3 : 1), text: `${id} result` }] }, task_id: null, state: null } satisfies SessionResult]));
    const entries = conversationEntries(team, sessions);
    expect(entries.map((item) => item.user || item.event?.text)).toEqual(["Initial request", "a result", "Follow up", "b result"]);
    expect(entries.find((item) => item.id === "m")?.queued).toBe(true);
    expect(conversationEntries(team, sessions, "b").map((item) => item.user || item.event?.text)).toEqual(["Initial request", "Follow up", "b result"]);
  });
  it("keeps previous replies while a later real response streams", () => {
    const team = makeTeam();
    team.agents[0] = { ...team.agents[0], status: "running", output: "Current partial" };
    team.events = [{ at: at(4), agent_id: "a", kind: "agent_started", message: "started" }];
    const sessions = { a: { session: { id: "session-a", name: null, events: [{ id: "first", type: "assistant_message", at: at(1), text: "First reply" }] }, task_id: null, state: null } satisfies SessionResult };
    expect(conversationEntries(team, sessions).filter((item) => item.event).map((item) => [item.event?.text, !!item.partial])).toEqual([["First reply", false], ["Current partial", true]]);
  });
  it("renders parallel peers in the same relay stage", () => {
    expect(agentStages([agent("first"), agent("parallel"), agent("verify", ["first", "parallel"]), agent("finish", ["verify"])]).map((stage) => stage.map((member) => member.id))).toEqual([["first", "parallel"], ["verify"], ["finish"]]);
  });
  it("does not announce a pending write as a delivered file", () => {
    const team = makeTeam();
    team.agents[0].status = "running";
    const sessions: Record<string, SessionResult> = { a: { session: { id: "session-a", name: null, events: [{ id: "write", type: "tool_call", at: at(1), name: "write_file", call_id: "c", arguments: { path: "report.md" } }] }, task_id: null, state: null } satisfies SessionResult };
    const view = render(<ProcessChat team={team} sessions={sessions} profiles={[]} onSelectAgent={vi.fn()} />);
    expect(screen.queryByText("report.md", { selector: "code" })).toBeNull();
    sessions.a.session.events.push({ id: "result", type: "tool_result", at: at(2), call_id: "c", ok: true, content: "written" });
    view.rerender(<ProcessChat team={team} sessions={sessions} profiles={[]} onSelectAgent={vi.fn()} />);
    expect(screen.getByText("report.md", { selector: "code" })).toBeTruthy();
  });
  it("subtracts already saved text from the current streamed turn", () => {
    const team = makeTeam();
    team.agents[0] = { ...team.agents[0], status: "running", output: "Reading first.\n\nNew partial reply" };
    team.events = [{ at: at(4), agent_id: "a", kind: "agent_started", message: "started" }];
    const sessions = { a: { session: { id: "session-a", name: null, events: [{ id: "first", type: "assistant_message", at: at(5), text: "Reading first." }, { id: "tool", type: "tool_call", at: at(6), name: "read_file" }] }, task_id: null, state: null } satisfies SessionResult };
    expect(conversationEntries(team, sessions).find((item) => item.partial)?.event?.text).toBe("New partial reply");
    expect(conversationEntries(team, sessions).at(-1)?.partial).toBe(true);
  });
  it("uses the frozen running turn identity even if settings change", () => {
    const team = makeTeam();
    team.agents[0] = { ...team.agents[0], status: "running", output: "Live text" };
    team.events = [{ at: at(4), agent_id: "a", kind: "agent_started", message: "started" }];
    const sessions = { a: { session: { id: "session-a", name: null, events: [{ id: "start", type: "turn_start", at: at(4), profile_id: "gpt", model_id: "gpt-original", family: "gpt" }] }, task_id: null, state: null } satisfies SessionResult };
    const partial = conversationEntries(team, sessions, undefined, [{ id: "gpt", name: "Updated", model: "gemini-changed", family: "gemini", protocol: "chat_completions" }]).find((entry) => entry.partial);
    expect(partial?.identity).toMatchObject({ modelId: "gpt-original", family: "gpt" });
  });
  it("separates admin assignments and unknown legacy instructions from human messages", () => {
    const team = makeTeam();
    team.admin = { session_id: "admin", profile_id: "gpt", model_id: "actual", family: "gpt", phase: "delegated", response: "" };
    team.messages = [{ id: "generated", agent_id: "a", text: "Analyze independently", at: at(2), source: "admin" }, { id: "legacy", agent_id: "b", text: "Unlabelled historic instruction", at: at(3) }, { id: "human", agent_id: "a", text: "My follow-up", at: at(4), source: "user" }];
    const view = render(<ProcessChat team={team} sessions={{}} profiles={[]} onSelectAgent={vi.fn()} />);
    expect(view.container.querySelectorAll(".lp-user-message")).toHaveLength(2); // Original human goal and actual follow-up.
    expect(screen.getByText("ARIS · 分工安排 · a")).toBeTruthy();
    expect(screen.getByText("任务指令 · b")).toBeTruthy();
    expect(view.container.querySelectorAll(".lp-dispatch-note")).toHaveLength(2);
  });
  it("shows actual admin follow-ups, deduplicating only the first original goal", () => {
    const team = makeTeam(); team.messages = [];
    team.admin = { session_id: "session-a", profile_id: "gpt", model_id: "actual", family: "gpt", phase: "answered", response: "" };
    const sessions: Record<string, SessionResult> = { a: { session: { id: "session-a", name: null, events: [
      { id: "u1", type: "user_message", at: at(1), text: team.goal },
      { id: "reply", type: "assistant_message", at: at(2), text: "Actual answer" },
      { id: "u2", type: "user_message", at: at(3), text: "Explain the next step" },
      { id: "u3", type: "user_message", at: at(4), text: team.goal },
    ] }, task_id: null, state: "completed" } };
    expect(conversationEntries(team, sessions, "a").filter((entry) => entry.user).map((entry) => entry.user)).toEqual([team.goal, "Explain the next step", team.goal]);
    team.messages = [{ id: "mirrored", at: at(3), text: "Explain the next step", source: "user" }];
    expect(conversationEntries(team, sessions, "a").filter((entry) => entry.user === "Explain the next step")).toHaveLength(1);
  });
  it("keeps selected worker execution inputs folded and never calls them human messages", () => {
    const team = makeTeam(); team.messages = [];
    const sessions: Record<string, SessionResult> = { a: { session: { id: "session-a", name: null, events: [{ id: "internal", type: "user_message", at: at(1), text: "You are a in ARIS. Dependency results and execution instructions." }] }, task_id: null, state: "completed" } };
    const view = render(<ProcessChat team={team} sessions={sessions} profiles={[]} agentId="a" onSelectAgent={vi.fn()} />);
    expect(view.container.querySelectorAll(".lp-user-message")).toHaveLength(1);
    expect(screen.getByText("a · 执行上下文")).toBeTruthy();
    expect(conversationEntries(team, sessions).some((entry) => entry.noteLabel === "执行上下文")).toBe(false);
  });
  it("ignores a stale same-role snapshot from a different team's session", () => {
    const team = makeTeam();
    const sessions: Record<string, SessionResult> = { a: { session: { id: "another-session", name: null, events: [{ id: "old", type: "assistant_message", at: at(1), text: "Other team's reply" }] }, task_id: null, state: "completed" } };
    expect(conversationEntries(team, sessions).some((entry) => entry.event?.text === "Other team's reply")).toBe(false);
  });
});
