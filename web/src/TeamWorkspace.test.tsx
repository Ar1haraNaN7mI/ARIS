import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { TeamWorkspace } from "./TeamWorkspace";
import type { Team, TeamAgent } from "./teamTypes";

const mocks = vi.hoisted(() => ({ state: {} as Record<string, unknown>, harness: vi.fn(), admin: vi.fn(), message: vi.fn(), run: vi.fn(), create: vi.fn(), select: vi.fn() }));
vi.mock("./useTeams", () => ({ useTeams: () => mocks.state }));
vi.mock("./useHarness", () => ({ useHarness: mocks.harness }));
vi.mock("./useTeamActivity", () => ({ useTeamActivity: () => ({ sessions: {}, approvals: [], error: "" }) }));
vi.mock("./api", () => ({ rpc: vi.fn().mockResolvedValue({ templates: [] }) }));
vi.mock("./SettingsCenter", () => ({ SettingsCenter: () => null }));
vi.mock("./WorkspaceTrajectory", () => ({ WorkspaceTrajectory: () => null }));
const worker: TeamAgent = { id: "research", name: "Research", profile_id: "astra", role: "research", prompt: "", files: [], depends_on: [], status: "completed", session_id: "worker-session" };
const base: Team = { id: "t", name: "Question", goal: "Original", created_at: "2026-11-19T00:00:00Z", status: "completed", agents: [worker], files: [], handoffs: [], events: [], checkpoints: [] };
beforeEach(() => {
  mocks.harness.mockReturnValue({ data: { token: "test", workspace: "/workspace" } });
  mocks.admin.mockResolvedValue(base); mocks.message.mockResolvedValue(base);
  mocks.state = { teams: [base], team: base, config: { configured: true, profiles: [{ id: "astra", name: "Astra", model: "actual-astra", family: "gpt" }] }, select: mocks.select,
    admin: mocks.admin, message: mocks.message, run: mocks.run, create: mocks.create, loading: false, busy: false, error: "" };
});
afterEach(() => { cleanup(); vi.clearAllMocks(); });
it("routes a no-role home message through admin exactly once without a second run", async () => {
  render(<TeamWorkspace onChat={vi.fn()} />);
  fireEvent.change(screen.getByLabelText("新消息"), { target: { value: "Explain the result" } });
  fireEvent.keyDown(screen.getByLabelText("新消息"), { key: "Enter" });
  await waitFor(() => expect(mocks.admin).toHaveBeenCalledWith("Explain the result", undefined, undefined));
  expect(mocks.admin).toHaveBeenCalledTimes(1); expect(mocks.run).not.toHaveBeenCalled(); expect(mocks.create).not.toHaveBeenCalled();
  expect(mocks.harness).toHaveBeenCalledWith(undefined, undefined, false);
});
it("sends a selected worker's continuation directly to its original session route", async () => {
  const view = render(<TeamWorkspace onChat={vi.fn()} />);
  fireEvent.click(view.container.querySelector(".lp-session-row")!);
  fireEvent.change(screen.getByLabelText("发送给 Research"), { target: { value: "Clarify your estimate" } });
  fireEvent.keyDown(screen.getByLabelText("发送给 Research"), { key: "Enter" });
  await waitFor(() => expect(mocks.message).toHaveBeenCalledWith("t", "Clarify your estimate", "research"));
  expect(mocks.admin).not.toHaveBeenCalled(); expect(mocks.run).not.toHaveBeenCalled();
});
it("keeps a simple answer as one normal admin conversation without subroles", async () => {
  const simple: Team = { ...base, agents: [{ ...worker, id: "admin", role: "admin", session_id: "admin-session" }], admin: { session_id: "admin-session", profile_id: "astra", model_id: "actual-astra", family: "gpt", phase: "answered", response: "Direct answer" } };
  mocks.state = { ...mocks.state, team: simple, teams: [simple] };
  const view = render(<TeamWorkspace onChat={vi.fn()} />);
  fireEvent.click(view.container.querySelector(".lp-group-heading")!);
  expect(view.container.querySelectorAll(".lp-session-row")).toHaveLength(0);
  expect(screen.queryByText(/个子任务/)).toBeNull(); expect(screen.getByText("Direct answer")).toBeTruthy();
  fireEvent.change(screen.getByLabelText("发送给ARIS"), { target: { value: "Continue" } });
  fireEvent.keyDown(screen.getByLabelText("发送给ARIS"), { key: "Enter" });
  await waitFor(() => expect(mocks.admin).toHaveBeenCalledWith("Continue", "t", undefined));
  expect(mocks.message).not.toHaveBeenCalled();
});
