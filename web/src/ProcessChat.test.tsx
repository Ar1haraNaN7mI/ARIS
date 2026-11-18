import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { conversationEntries, ProcessChat } from "./ProcessChat";
import { agentStages } from "./ProcessRelay";
import type { Team, TeamAgent } from "./teamTypes";
import type { SessionResult } from "./types";

const at = (second: number) => `2026-11-19T00:00:${String(second).padStart(2, "0")}Z`;
afterEach(cleanup);
const agent = (id: string, deps: string[] = []): TeamAgent => ({ id, name: id, role: id, profile_id: "gpt", prompt: "", files: [], depends_on: deps, status: "completed" });
const makeTeam = (): Team => ({ id: "t", name: "Task", goal: "Initial request", created_at: at(0), status: "completed", agents: [agent("a"), agent("b")], files: [], events: [], handoffs: [], checkpoints: [], messages: [{ id: "m", text: "Follow up", at: at(2), queued: true }] });
describe("Process conversation uses real chronological records", () => {
  it("interleaves historical replies with user messages across agents", () => {
    const team = makeTeam();
    const sessions = Object.fromEntries(["a", "b"].map((id, index) => [id, { session: { id, name: id, events: [{ id: "reply", type: "assistant_message", at: at(index ? 3 : 1), text: `${id} result` }] }, task_id: null, state: null } satisfies SessionResult]));
    const entries = conversationEntries(team, sessions);
    expect(entries.map((item) => item.user || item.event?.text)).toEqual(["Initial request", "a result", "Follow up", "b result"]);
    expect(entries.find((item) => item.id === "m")?.queued).toBe(true);
    expect(conversationEntries(team, sessions, "b").map((item) => item.user || item.event?.text)).toEqual(["Initial request", "Follow up", "b result"]);
  });
  it("keeps previous replies while a later real response streams", () => {
    const team = makeTeam();
    team.agents[0] = { ...team.agents[0], status: "running", output: "Current partial" };
    team.events = [{ at: at(4), agent_id: "a", kind: "agent_started", message: "started" }];
    const sessions = { a: { session: { id: "a", name: null, events: [{ id: "first", type: "assistant_message", at: at(1), text: "First reply" }] }, task_id: null, state: null } satisfies SessionResult };
    expect(conversationEntries(team, sessions).filter((item) => item.event).map((item) => [item.event?.text, !!item.partial])).toEqual([["First reply", false], ["Current partial", true]]);
  });
  it("renders parallel peers in the same relay stage", () => {
    expect(agentStages([agent("first"), agent("parallel"), agent("verify", ["first", "parallel"]), agent("finish", ["verify"])]).map((stage) => stage.map((member) => member.id))).toEqual([["first", "parallel"], ["verify"], ["finish"]]);
  });
  it("does not announce a pending write as a delivered file", () => {
    const team = makeTeam();
    team.agents[0].status = "running";
    const sessions: Record<string, SessionResult> = { a: { session: { id: "a", name: null, events: [{ id: "write", type: "tool_call", at: at(1), name: "write_file", call_id: "c", arguments: { path: "report.md" } }] }, task_id: null, state: null } satisfies SessionResult };
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
    const sessions = { a: { session: { id: "a", name: null, events: [{ id: "first", type: "assistant_message", at: at(5), text: "Reading first." }, { id: "tool", type: "tool_call", at: at(6), name: "read_file" }] }, task_id: null, state: null } satisfies SessionResult };
    expect(conversationEntries(team, sessions).find((item) => item.partial)?.event?.text).toBe("New partial reply");
    expect(conversationEntries(team, sessions).at(-1)?.partial).toBe(true);
  });
  it("uses the frozen running turn identity even if settings change", () => {
    const team = makeTeam();
    team.agents[0] = { ...team.agents[0], status: "running", output: "Live text" };
    team.events = [{ at: at(4), agent_id: "a", kind: "agent_started", message: "started" }];
    const sessions = { a: { session: { id: "a", name: null, events: [{ id: "start", type: "turn_start", at: at(4), profile_id: "gpt", model_id: "gpt-original", family: "gpt" }] }, task_id: null, state: null } satisfies SessionResult };
    const partial = conversationEntries(team, sessions, undefined, [{ id: "gpt", name: "Updated", model: "gemini-changed", family: "gemini", protocol: "chat_completions" }]).find((entry) => entry.partial);
    expect(partial?.identity).toMatchObject({ modelId: "gpt-original", family: "gpt" });
  });
});
