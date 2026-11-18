import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { MessageComposer, TeamTimeline } from "./TeamConversation";
import type { Team, TeamAgent } from "./teamTypes";
import type { SessionResult } from "./types";
afterEach(cleanup);
const agent: TeamAgent = { id: "review", name: "审查员", role: "reviewer", profile_id: "gpt", prompt: "Review", depends_on: [], files: [], status: "completed" };
describe("Team and agent conversation boundaries", () => {
  it("sends a team message and clears it only after acceptance", async () => {
    const send = vi.fn().mockResolvedValue({ id: "team" });
    render(<MessageComposer running={false} busy={false} onSend={send} />);
    fireEvent.change(screen.getByLabelText("发送给整个团队"), { target: { value: "  补充来源证据  " } });
    fireEvent.click(screen.getByRole("button", { name: "发送并继续执行" }));
    await waitFor(() => expect(send).toHaveBeenCalledWith("补充来源证据"));
    await waitFor(() => expect((screen.getByLabelText("发送给整个团队") as HTMLTextAreaElement).value).toBe(""));
  });
  it("keeps a rejected sub-agent instruction for correction", async () => {
    render(<MessageComposer agent={agent} running={false} busy={false} onSend={vi.fn().mockResolvedValue(undefined)} />);
    fireEvent.change(screen.getByLabelText("发送给 审查员"), { target: { value: "仅复查第二部分" } });
    fireEvent.click(screen.getByRole("button", { name: "发送并继续执行" }));
    await waitFor(() => expect((screen.getByLabelText("发送给 审查员") as HTMLTextAreaElement).value).toBe("仅复查第二部分"));
  });
  it("accepts a queued message while a team is executing", async () => {
    const send = vi.fn().mockResolvedValue({ id: "team" });
    render(<MessageComposer agent={agent} running busy={false} onSend={send} />);
    const input = screen.getByLabelText("发送给 审查员");
    expect((input as HTMLTextAreaElement).disabled).toBe(false);
    fireEvent.change(input, { target: { value: "下一轮补充来源" } });
    fireEvent.keyDown(input, { key: "Enter", shiftKey: true });
    expect(send).not.toHaveBeenCalled();
    fireEvent.keyDown(input, { key: "Enter" });
    await waitFor(() => expect(send).toHaveBeenCalledWith("下一轮补充来源"));
  });
  it("inserts only actual shared file paths", () => {
    render(<MessageComposer running={false} busy={false} files={["notes/research.md"]} onSend={vi.fn()} />);
    fireEvent.click(screen.getByRole("button", { name: "@ 引用文件" }));
    fireEvent.click(screen.getByRole("option", { name: "notes/research.md" }));
    expect((screen.getByLabelText("发送给整个团队") as HTMLTextAreaElement).value).toBe("@notes/research.md ");
  });
  it("keeps the draft when the recipient changes", () => {
    const send = vi.fn();
    const view = render(<MessageComposer running={false} busy={false} onSend={send} />);
    fireEvent.change(screen.getByLabelText("发送给整个团队"), { target: { value: "不要丢失这条草稿" } });
    view.rerender(<MessageComposer agent={agent} running={false} busy={false} onSend={send} />);
    expect((screen.getByLabelText("发送给 审查员") as HTMLTextAreaElement).value).toBe("不要丢失这条草稿");
  });
  it("keeps actual tool results available in the complete execution trace", () => {
    const team = { id: "team", agents: [agent], events: [{ at: "2026-11-19T00:00:00Z", kind: "agent_started", message: "审查员开始", agent_id: "review" }] } as Team;
    const sessions = { review: { session: { events: [{ id: "tool-1", type: "tool_result", at: "2026-11-19T00:00:01Z", ok: false, content: "file missing" }] } } as SessionResult };
    render(<TeamTimeline team={team} sessions={sessions} onSelectAgent={vi.fn()} />);
    fireEvent.click(screen.getByRole("tab", { name: "工具" }));
    expect(screen.queryByText("审查员开始")).toBeNull();
    expect(screen.getByText("工具返回 · 失败")).toBeTruthy();
    fireEvent.click(screen.getByText("查看参数与结果"));
    expect(screen.getByText("file missing")).toBeTruthy();
  });
});
