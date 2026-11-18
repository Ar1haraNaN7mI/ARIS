import { act, cleanup, renderHook, waitFor } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import { useTeamActivity } from "./useTeamActivity";
import * as api from "./api";
import type { Team } from "./teamTypes";
vi.mock("./api", async (original) => ({ ...(await original<typeof import("./api")>()), rpc: vi.fn() }));
afterEach(() => { cleanup(); vi.resetAllMocks(); });

it("shows only approvals belonging to this team's actual session tasks", async () => {
  const team = { id: "team-a", agents: [{ id: "agent-a", session_id: "session-a" }] } as Team;
  vi.mocked(api.rpc).mockImplementation(async (_token, method) => {
    if (method === "sessions/get") return { task_id: "task-a", session: { events: [] } };
    if (method === "approvals/list") return { approvals: [{ request_id: "own", task_id: "task-a", name: "run_shell" }, { request_id: "other", task_id: "task-b", name: "run_shell" }, { request_id: "unscoped", task_id: null, name: "run_shell" }] };
    return {};
  });
  const { result } = renderHook(() => useTeamActivity("token", team));
  await waitFor(() => expect(result.current.approvals.map((item) => item.request_id)).toEqual(["own"]));
  await act(async () => result.current.resolve("own", true));
  expect(api.rpc).toHaveBeenCalledWith("token", "approvals/resolve", { request_id: "own", allow: true });
  expect(result.current.approvals).toEqual([]);
});
