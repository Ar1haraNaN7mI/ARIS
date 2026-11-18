import { describe, expect, it } from "vitest";
import { render } from "@testing-library/react";
import { SessionTimeline, sessionTimeline } from "./AgentTranscript";
import type { SessionEvent } from "./types";

const event = (id: string, type: string, extra: Partial<SessionEvent> = {}): SessionEvent => ({ id, type, at: "2026-11-19T00:00:00Z", ...extra });

describe("agent execution history", () => {
  it("pairs interleaved tools and keeps reused call IDs in separate turns", () => {
    const items = sessionTimeline([
      event("start-1", "turn_start"),
      event("call-a", "tool_call", { call_id: "a", name: "read_file" }),
      event("call-b", "tool_call", { call_id: "b", name: "web_crawl" }),
      event("result-b", "tool_result", { call_id: "b", content: "page", ok: true }),
      event("result-a", "tool_result", { call_id: "a", content: "first", ok: true }),
      event("end-1", "turn_end"),
      event("start-2", "turn_start"),
      event("call-a2", "tool_call", { call_id: "a", name: "read_file" }),
      event("result-a2", "tool_result", { call_id: "a", content: "second", ok: false }),
    ]);
    expect(items.filter((item) => item.kind === "tool").map((item) => item.kind === "tool" && [item.event.id, item.result?.content])).toEqual([
      ["call-a", "first"], ["call-b", "page"], ["call-a2", "second"],
    ]);
  });

  it("does not label a tool with no result as active after its turn ended", () => {
    const [item] = sessionTimeline([event("call", "tool_call", { call_id: "lost" }), event("end", "turn_end")]);
    expect(item.kind === "tool" && item.turnEnded).toBe(true);
    expect(item.kind === "tool" && item.result).toBeUndefined();
  });

  it("shows the initial goal and the latest instruction for each turn without exposing the full harness prompt", () => {
    const { container } = render(<SessionTimeline agentName="研究员" goal="核对产品资料" events={[
      event("initial", "user_message", { text: "FULL_INITIAL_HARNESS_CONTEXT", at: "2026-11-19T00:00:00Z" }),
      event("followup", "user_message", { text: "FULL_FOLLOWUP_HARNESS_CONTEXT", at: "2026-11-19T00:02:00Z" }),
      event("answer", "assistant_message", { text: "已更新资料。", at: "2026-11-19T00:02:30Z" }),
    ]} messages={[
      { id: "future", text: "未来的指令", at: "2026-11-19T00:03:00Z" },
      { id: "latest", text: "只更新价格来源", at: "2026-11-19T00:01:30Z" },
      { id: "older", text: "核对发布日期", at: "2026-11-19T00:01:00Z" },
    ]} />);
    const turns = container.querySelectorAll(".user_message");
    expect(turns[0].querySelector(":scope > .message-body")?.textContent).toBe("核对产品资料");
    expect(turns[1].querySelector(":scope > .message-body")?.textContent).toBe("只更新价格来源");
    expect(turns[1].querySelector("header span")?.textContent).toBe("你");
    expect(turns[1].querySelector("details")?.hasAttribute("open")).toBe(false);
    expect(turns[1].querySelector("details")?.textContent).toContain("FULL_FOLLOWUP_HARNESS_CONTEXT");
    expect(container.querySelector(".assistant_message .message-body")?.textContent).toBe("已更新资料。");
    expect(container.textContent).not.toContain("未来的指令");
  });
});
