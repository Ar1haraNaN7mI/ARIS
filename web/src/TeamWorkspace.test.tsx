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
it("opens the coordinator conversation and gives each spawned worker a peer lane with its own target", async () => {
  const coordinator = { ...worker, id: "admin", name: "ARIS", role: "admin", session_id: "controller-session" };
  const writer = { ...worker, id: "writer", name: "Writer", session_id: "writer-session", subagent_id: "sub-writer", depends_on: [worker.id] };
  const team: Team = { ...base, agents: [coordinator, { ...worker, subagent_id: "sub-research" }, writer],
    admin: { session_id: coordinator.session_id, profile_id: "astra", model_id: "actual-astra", family: "gpt", phase: "delegated", response: "I assigned both workers." } };
  mocks.admin.mockImplementationOnce(async () => { mocks.state = { ...mocks.state, teams: [team], team }; return team; });
  const view = render(<TeamWorkspace onChat={vi.fn()} />);
  fireEvent.change(screen.getByLabelText("新消息"), { target: { value: "Delegate two tasks" } });
  fireEvent.keyDown(screen.getByLabelText("新消息"), { key: "Enter" });
  await waitFor(() => expect(view.container.querySelector('[data-relay-agent="admin"]')?.getAttribute("aria-pressed")).toBe("true"));
  const rows = view.container.querySelectorAll(".lp-relay-row");
  expect(rows).toHaveLength(3);
  expect(Array.from(rows).map((row) => row.getAttribute("data-relay-session"))).toEqual(["controller-session", "worker-session", "writer-session"]);
  expect(Array.from(rows).every((row) => row.querySelectorAll("[data-relay-agent]").length === 1)).toBe(true);
  fireEvent.click(rows[2].querySelector("button")!);
  expect(screen.getByLabelText("发送给 Writer")).toBeTruthy();
  expect(screen.queryByText("I assigned both workers.")).toBeNull();
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

it("retries the same administrator after planning failed before any workers existed", async () => {
  const team: Team = { ...base, status: "failed", agents: [], admin: {
    session_id: "admin-session", profile_id: "astra", model_id: "actual-astra", family: "gpt", phase: "failed", response: "",
  } };
  mocks.state = { ...mocks.state, team, teams: [team] };
  const view = render(<TeamWorkspace onChat={vi.fn()} />);
  fireEvent.click(view.container.querySelector(".lp-group-heading")!);
  fireEvent.click(screen.getByRole("button", { name: /▶ 继续/ }));
  expect(mocks.admin).toHaveBeenCalledWith("继续完成上一次未完成的请求。", team.id, undefined);
  expect(mocks.run).not.toHaveBeenCalled();
});
