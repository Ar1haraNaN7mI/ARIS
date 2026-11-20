import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import { rpc } from "./api";
import { WorkflowReview } from "./WorkflowReview";
import { ChatWorkspace } from "./ChatWorkspace";

vi.mock("./api", () => ({ rpc: vi.fn(), errorText: (error: Error) => error.message }));
const harness = vi.hoisted(() => vi.fn());
vi.mock("./useHarness", () => ({ useHarness: harness }));
vi.mock("./SettingsCenter", () => ({ SettingsCenter: () => null }));
const read = vi.mocked(rpc);
const current = {
  id: "r2", status: "completed", model: "review-model", summary: "收尾流程稳定，更新了并发调度规则。",
  findings: [{ evidence: "两个就绪任务等了同一把锁", lesson: "独立任务可以同时开始" }],
  changes: [{ target: "scheduler", description: "优先调度已就绪的独立任务", status: "applied", baseline: "2 次等待", candidate: "1 次等待", version: 2 }],
};
afterEach(() => { cleanup(); vi.resetAllMocks(); vi.useRealTimers(); });

it("follows a completed task from its queued review to a graded change without user input", async () => {
  vi.useFakeTimers();
  const queued = { ...current, status: "queued", changes: [] };
  const running = { ...queued, status: "running" };
  read.mockResolvedValueOnce({ review: queued }).mockResolvedValueOnce({ review: running }).mockResolvedValue({ review: current });
  render(<WorkflowReview token="token" subject="team_id" id="team-a" />);
  await act(() => Promise.resolve());
  expect(screen.getByText("等待复盘")).toBeTruthy();
  await act(() => vi.advanceTimersByTimeAsync(1500));
  expect(screen.getByText("正在复盘")).toBeTruthy();
  await act(() => vi.advanceTimersByTimeAsync(1500));
  expect(screen.getByText("已完成 · 1 项改进已生效")).toBeTruthy();
  fireEvent.click(screen.getByText("工作流复盘"));
  expect(screen.getByText("独立任务可以同时开始")).toBeTruthy();
  expect(screen.getByText("两个就绪任务等了同一把锁")).toBeTruthy();
  expect(screen.getByLabelText("工作流回放").textContent).toContain("候选回放1 次等待");
  expect(screen.getByText("后续适用任务采用 · v2")).toBeTruthy();
  await act(() => vi.advanceTimersByTimeAsync(20_000));
  expect(read).toHaveBeenCalledTimes(3);
  expect(read.mock.calls.every(([, method, params]) => method === "reviewer/get" && params?.team_id === "team-a")).toBe(true);
});

it("keeps past decisions and failed reviews readable in a regular chat", async () => {
  const previous = { ...current, id: "r1", status: "failed", error: "模型返回无效的评测结果", summary: "", findings: [], changes: [] };
  read.mockResolvedValue({ review: current, history: [current, previous] });
  render(<WorkflowReview token="token" subject="session_id" id="session-a" />);
  fireEvent.click(await screen.findByText("工作流复盘"));
  fireEvent.change(screen.getByLabelText("复盘记录"), { target: { value: previous.id } });
  expect(screen.getByText(previous.error)).toBeTruthy();
  expect(screen.queryByText("后续适用任务采用 · v2")).toBeNull();
  expect(read).toHaveBeenCalledWith("token", "reviewer/get", { session_id: "session-a" }, expect.any(AbortSignal));
});

it("reveals the native execution evidence cited by the reviewer", async () => {
  const review = { ...current, evidence: { turn: { tool_outcomes: { "read_file/ok": 1 } } },
    findings: [{ evidence: "turn", lesson: "渠道记录读取成功" }] };
  read.mockResolvedValue({ review });
  render(<WorkflowReview token="token" subject="team_id" id="example" />);
  fireEvent.click(await screen.findByText("工作流复盘"));
  fireEvent.click(screen.getByText("查看执行证据 · turn"));
  expect(await screen.findByText(/read_file\/ok/)).toBeTruthy();
});

it("distinguishes observed tool outcomes from scheduler replay when a rule is retired", async () => {
  const change = { target: "recovery_rule", status: "retired", description: "跨任务反例使规则停用", baseline: "上次执行成功", candidate: "本次相同操作连续失败", version: 3 };
  read.mockResolvedValue({ review: { ...current, changes: [change] } });
  render(<WorkflowReview token="token" subject="team_id" id="example" />);
  fireEvent.click(await screen.findByText("工作流复盘"));
  expect(screen.getByText("已停用 · v3")).toBeTruthy();
  expect(screen.getByLabelText("执行记录对比").textContent).toContain("验证记录本次相同操作连续失败");
  expect(screen.queryByLabelText("工作流回放")).toBeNull();
});

it("does not attach an older task's delayed review to the newly selected task", async () => {
  let resolve!: (value: unknown) => void;
  read.mockImplementationOnce(() => new Promise((done) => { resolve = done; }))
    .mockResolvedValue({ review: null, history: [] });
  const view = render(<WorkflowReview token="token" subject="task_id" id="old" />);
  const signal = read.mock.calls[0][3];
  view.rerender(<WorkflowReview token="token" subject="task_id" id="new" />);
  await act(async () => { resolve({ review: current }); });
  expect(signal?.aborted).toBe(true);
  await waitFor(() => expect(read).toHaveBeenCalledWith("token", "reviewer/get", { task_id: "new" }, expect.any(AbortSignal)));
  expect(screen.queryByText("工作流复盘")).toBeNull();
});

it("reconciles when the previous report reaches the UI before the new review is queued", async () => {
  vi.useFakeTimers();
  const previous = { ...current, id: "r1", completed_at: "2026-11-01T11:00:00Z" };
  read.mockResolvedValueOnce({ review: previous, history: [previous] }).mockResolvedValue({ review: { ...current, completed_at: "2026-11-01T12:01:00Z" } });
  render(<WorkflowReview token="token" subject="session_id" id="same" startedAt="2026-11-01T12:00:00Z" />);
  await act(() => Promise.resolve());
  expect(screen.queryByText("工作流复盘")).toBeNull();
  await act(() => vi.advanceTimersByTimeAsync(2000));
  expect(screen.getByText("工作流复盘")).toBeTruthy();
  expect(read).toHaveBeenCalledTimes(2);
});

it("finds a review after a large workflow finishes collecting its evidence", async () => {
  vi.useFakeTimers();
  read.mockResolvedValue({ review: null });
  render(<WorkflowReview token="token" subject="team_id" id="team" />);
  await act(() => vi.advanceTimersByTimeAsync(60_000));
  expect(read.mock.calls.length).toBeGreaterThan(4);
  read.mockResolvedValue({ review: current });
  await act(() => vi.advanceTimersByTimeAsync(60_000));
  expect(screen.getByText("工作流复盘")).toBeTruthy();
});

it("attaches the session review to the completed answer in independent chat", async () => {
  read.mockResolvedValue({ review: current });
  harness.mockReturnValue({
    data: { token: "token", workspace: "/workspace", model: { name: "KEI", ready: true }, profile: { username: "审阅", badge_id: "AR" }, skills: [], plugins: [] },
    sessions: [], approvals: [], notices: [], currentId: "chat",
    snapshot: { task_id: "task", state: "completed", session: { id: "chat", events: [
      { type: "assistant_message", text: "任务已完成", id: "m", at: "2026-11-01T12:00:00Z" },
      { type: "turn_end", id: "turn", at: "2026-11-01T12:00:01Z" },
    ] } },
  });
  render(<ChatWorkspace onTeams={vi.fn()} />);
  fireEvent.click(await screen.findByText("工作流复盘"));
  expect(screen.getByText("任务已完成")).toBeTruthy();
  expect(read).toHaveBeenCalledWith("token", "reviewer/get", { session_id: "chat" }, expect.any(AbortSignal));
});
