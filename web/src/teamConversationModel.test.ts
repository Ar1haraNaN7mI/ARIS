import { expect, it } from "vitest";
import { ADMIN_VIEW, adminAgent, conversationAgents, isAdminOnly, messageSource, progressAgents, workerAgents } from "./teamConversationModel";
import { relayStages } from "./ProcessRelay";
import { conversationEntries } from "./ProcessChat";
import { stableTeams } from "./useTeams";
import type { Team, TeamAgent } from "./teamTypes";
const worker: TeamAgent = { id: "research", name: "Research", profile_id: "gpt", role: "research", prompt: "", files: [], depends_on: [], status: "completed", session_id: "worker-session" };
const team: Team = { id: "t", name: "Chat", goal: "Hello", created_at: "2026-11-19T00:00:00Z", status: "completed", agents: [], files: [], handoffs: [], events: [], checkpoints: [], admin: { session_id: "admin-session", profile_id: "gpt", model_id: "actual-model", family: "gpt", phase: "answered", response: "Hello back" } };
it("shows the single persisted admin conversation without counting a fake subagent", () => {
  const simple = { ...team, agents: [{ ...worker, id: "admin", role: "admin", session_id: "admin-session" }] };
  expect(isAdminOnly(simple)).toBe(true);
  expect(workerAgents(simple)).toEqual([]);
  expect(conversationAgents(simple)).toHaveLength(1);
  expect(conversationAgents(simple)[0].session_id).toBe("admin-session");
  expect(conversationEntries(simple, {}).map((entry) => entry.user || entry.event?.text)).toEqual(["Hello", "Hello back"]);
});
it("keeps a template's actual workers while exposing its separate admin session", () => {
  const configured = { ...team, agents: [worker] };
  expect(workerAgents(configured)).toEqual([worker]);
  expect(adminAgent(configured)?.id).toBe(ADMIN_VIEW);
  expect(relayStages(configured).map((stage) => stage.map((agent) => agent.id))).toEqual([[ADMIN_VIEW], ["research"]]);
});
it("preserves unchanged team references while updating only a changed lane", () => {
  const previous = [team, { ...team, id: "other" }];
  expect(stableTeams(previous, structuredClone(previous))).toBe(previous);
  const next = stableTeams(previous, [structuredClone(team), { ...previous[1], status: "running" }]);
  expect(next[0]).toBe(previous[0]); expect(next[1]).not.toBe(previous[1]);
});
it("never attributes generated or legacy unlabelled assignments to the human", () => {
  const instruction = { id: "assignment", agent_id: "research", at: team.created_at, text: "Research the question" };
  expect(messageSource(team, instruction)).toBe("unknown");
  expect(messageSource(team, { ...instruction, source: "admin" })).toBe("admin");
  expect(messageSource(team, { ...instruction, source: "user" })).toBe("user");
  expect(messageSource({ ...team, admin: undefined }, instruction)).toBe("user");
});
it("counts only this run's chosen workers without erasing unselected roles", () => {
  const selected: Team = { ...team, agents: [worker, { ...worker, id: "later", status: "pending" }], execution_budget: { agent_ids: ["research"], max_agent_runs: 6, max_steps_per_agent: 6, max_tokens_per_step: 4096, max_parallel: 3, max_runtime_secs: 180 } };
  expect(progressAgents(selected).map((agent) => agent.id)).toEqual(["research"]);
  expect(workerAgents(selected)).toHaveLength(2);
  expect(selected.agents[1].status).toBe("pending");
});
