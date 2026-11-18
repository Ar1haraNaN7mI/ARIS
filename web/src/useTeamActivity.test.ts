import { act, cleanup, renderHook, waitFor } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import { useTeamActivity } from "./useTeamActivity";
import * as api from "./api";
import type { Team } from "./teamTypes";
vi.mock("./api", async (original) => ({ ...(await original<typeof import("./api")>()), rpc: vi.fn() }));
afterEach(() => { cleanup(); vi.useRealTimers(); vi.restoreAllMocks(); vi.resetAllMocks(); });

it("shows only approvals belonging to this team's actual session tasks", async () => {
  const team = { id: "team-a", agents: [{ id: "agent-a", session_id: "session-a" }] } as Team;
  vi.mocked(api.rpc).mockImplementation(async (_token, method) => {
    if (method === "sessions/get") return { task_id: "task-a", session: { id: "session-a", events: [] } };
    if (method === "approvals/list") return { approvals: [{ request_id: "own", task_id: "task-a", name: "run_shell" }, { request_id: "other", task_id: "task-b", name: "run_shell" }, { request_id: "unscoped", task_id: null, name: "run_shell" }] };
    return {};
  });
  const { result } = renderHook(() => useTeamActivity("token", team));
  await waitFor(() => expect(result.current.approvals.map((item) => item.request_id)).toEqual(["own"]));
  await act(async () => result.current.resolve("own", true));
  expect(api.rpc).toHaveBeenCalledWith("token", "approvals/resolve", { request_id: "own", allow: true });
  expect(result.current.approvals).toEqual([]);
});

it("polls only the one running session among eight, then stops after final synchronization", async () => {
  vi.useFakeTimers(); vi.spyOn(document, "hidden", "get").mockReturnValue(false);
  const team: Team = { id: "t", name: "Team", goal: "Work", files: [], handoffs: [], checkpoints: [], created_at: "2026-11-19T00:00:00Z", status: "running", events: [], agents: Array.from({ length: 8 }, (_, i) => ({ id: `a${i}`, name: `Agent ${i}`, role: "research", profile_id: "gpt", prompt: "", depends_on: [], files: [], session_id: `s${i}`, status: i === 0 ? "running" : "completed" })) };
  vi.mocked(api.rpc).mockImplementation(async (_token, method, params) => {
    if (method === "sessions/get") return { task_id: params?.id, state: "running", session: { id: params?.id, name: null, events: [] }, sync: { cursor: `cursor-${params?.id}`, reset: !params?.cursor, event_count: 0 } };
    return { approvals: [] };
  });
  const { result, rerender } = renderHook(({ value }) => useTeamActivity("token", value), { initialProps: { value: team } });
  await act(async () => {});
  const reads = () => vi.mocked(api.rpc).mock.calls.filter((call) => call[1] === "sessions/get");
  expect(reads()).toHaveLength(8);
  const retained = result.current.sessions.a1;
  await act(async () => { await vi.advanceTimersByTimeAsync(22_000); });
  expect(reads()).toHaveLength(18); // Previously 8 initial + 10 × 8 = 88.
  expect(reads().slice(8).every((call) => call[2]?.id === "s0" && call[2]?.cursor === "cursor-s0")).toBe(true);
  expect(result.current.sessions.a1).toBe(retained);
  const finished = { ...team, status: "completed" as const, agents: team.agents.map((agent) => ({ ...agent, status: "completed" as const })) };
  rerender({ value: finished }); await act(async () => {});
  expect(reads()).toHaveLength(19);
  await act(async () => { await vi.advanceTimersByTimeAsync(22_000); });
  expect(reads()).toHaveLength(19);
});

it("retains completed snapshots across team selection without refetching or clearing history", async () => {
  const one: Team = { id: "one", name: "One", goal: "Done", files: [], handoffs: [], checkpoints: [], created_at: "2026-11-19T00:00:00Z", status: "completed", events: [], agents: [{ id: "a", name: "A", role: "research", profile_id: "gpt", prompt: "", depends_on: [], files: [], session_id: "s-a", status: "completed" }] };
  const two = { ...one, id: "two", agents: [{ ...one.agents[0], id: "b", session_id: "s-b" }] };
  vi.mocked(api.rpc).mockImplementation(async (_token, method, params) => method === "sessions/get" ? { session: { id: params?.id, name: null, events: [] }, task_id: null, state: "completed" } : { approvals: [] });
  const { result, rerender } = renderHook(({ team }) => useTeamActivity("token", team), { initialProps: { team: one } });
  await waitFor(() => expect(result.current.sessions.a).toBeTruthy());
  const original = result.current.sessions.a;
  rerender({ team: two }); await waitFor(() => expect(result.current.sessions.b).toBeTruthy());
  rerender({ team: one }); await waitFor(() => expect(result.current.sessions.a).toBe(original));
  expect(vi.mocked(api.rpc).mock.calls.filter((call) => call[1] === "sessions/get")).toHaveLength(2);
});
