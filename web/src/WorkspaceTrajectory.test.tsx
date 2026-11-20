import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { WorkspaceTrajectory } from "./WorkspaceTrajectory";
import { projectWorkspaceTrace } from "./workspaceTrace";
import type { ModelProfile, Team, TeamAgent } from "./teamTypes";
import type { SessionEvent, SessionResult } from "./types";

// The animation transport needs browser layout/canvas; these tests exercise the
// real projection and interactive component, without running its visual effects.
vi.mock("./vendor/lancer-light-ui/src/index", () => ({
  createLightLayer: () => ({ activeCount: 0, emit: vi.fn(() => ({ cancel: vi.fn() })), destroy: vi.fn() }),
}));

beforeEach(() => {
  vi.stubGlobal("ResizeObserver", class {
    observe() {}
    unobserve() {}
    disconnect() {}
  });
});
afterEach(() => { cleanup(); vi.unstubAllGlobals(); });

const at = (second: number) => new Date(Date.UTC(2026, 10, 19, 0, 0, second)).toISOString();
const profiles: ModelProfile[] = [{ id: "same-model", name: "同一模型", model: "gpt-6.2-astra", family: "gpt", protocol: "chat_completions" }];
const role = (team: string, id: string, name: string, depends_on: string[] = []): TeamAgent => ({
  id, name, role: name, profile_id: "same-model", session_id: `${team}-session-${id}`,
  prompt: "实际任务", depends_on, files: [], status: "completed", output: "",
});
const event = (id: string, type: string, second: number, rest: Partial<SessionEvent> = {}): SessionEvent => ({ id, type, at: at(second), ...rest });
const session = (agent: TeamAgent, events: SessionEvent[]): SessionResult => ({ session: { id: agent.session_id!, name: agent.name, events }, task_id: null, state: "completed" });

function fixture(): { current: Team; archive: Team; sessions: Record<string, SessionResult> } {
  const agents = [role("current", "cost", "成本核算员"), role("current", "score", "机会评分员"), role("current", "writer", "决策主笔", ["cost", "score"])];
  const current: Team = {
    id: "current", name: "本次模拟复盘", goal: "并行核验后汇总", created_at: at(0), status: "completed", agents,
    files: [], handoffs: [], checkpoints: [], messages: [], events: [
      { at: at(0), agent_id: null, kind: "created", message: "Shared workspace created" },
      { at: at(1), agent_id: "cost", kind: "agent_started", message: "成本核算员" },
      { at: at(2), agent_id: "score", kind: "agent_started", message: "机会评分员" },
      { at: at(4), agent_id: "cost", kind: "agent_finished", message: "成本核算员: completed" },
      { at: at(5), agent_id: "score", kind: "agent_finished", message: "机会评分员: completed" },
      { at: at(6), agent_id: "writer", kind: "agent_started", message: "决策主笔" },
      { at: at(9), agent_id: "writer", kind: "agent_finished", message: "决策主笔: completed" },
      { at: at(10), agent_id: null, kind: "checkpoint", message: "保存对照版本" },
    ],
  };
  // Reusing a role ID in another task must never collapse its column or select
  // the current task's session merely because both use the same model.
  const archive: Team = { id: "archive", name: "另一项历史任务", goal: "旧记录", created_at: at(-20), status: "completed", agents: [role("archive", "cost", "历史核验员")], files: [], handoffs: [], checkpoints: [], events: [{ at: at(-10), agent_id: "cost", kind: "agent_finished", message: "历史核验员: completed" }] };
  const sessions = {
    cost: session(agents[0], [event("cost-start", "turn_start", 1, { profile_id: "same-model", model_id: "gpt-6.2-astra", family: "gpt" }), event("cost-reply", "assistant_message", 3, { text: "成本核算的实际证据" }), event("cost-end", "turn_end", 4)]),
    score: session(agents[1], [event("score-start", "turn_start", 2, { profile_id: "same-model", model_id: "gpt-6.2-astra", family: "gpt" }), event("score-reply", "assistant_message", 4, { text: "机会评分的实际证据" }), event("score-end", "turn_end", 5)]),
    writer: session(agents[2], [event("writer-start", "turn_start", 6, { profile_id: "same-model", model_id: "gpt-6.2-astra", family: "gpt" }), event("writer-reply", "assistant_message", 8, { text: "两项证据合并后的决策" }), event("writer-end", "turn_end", 9)]),
  };
  return { current, archive, sessions };
}

const headers = (container: HTMLElement) => Array.from(container.querySelectorAll<HTMLButtonElement>(".wt-lane-head[data-lane-team][data-lane-agent]"));

describe("role-owned workspace trajectory", () => {
  it("keeps three roles on the same model distinct and selects each exact role", () => {
    const { current, archive, sessions } = fixture(); const select = vi.fn();
    const view = render(<WorkspaceTrajectory teams={[current, archive]} activeTeamId={current.id} profiles={profiles} sessions={sessions} onSelectAgent={select} />);
    const buttons = headers(view.container);
    expect(buttons).toHaveLength(3);
    expect(buttons.map((button) => button.dataset.laneAgent)).toEqual(["cost", "score", "writer"]);
    for (const agent of current.agents) {
      const button = buttons.find((candidate) => candidate.dataset.laneAgent === agent.id)!;
      expect(button.dataset.laneTeam).toBe(current.id);
      expect(`${button.title} ${button.getAttribute("aria-label") || ""}`).toContain(agent.name);
      expect(button.textContent).toContain(agent.name);
      expect(button.textContent).toContain("gpt-6.2-astra");
      fireEvent.click(button);
    }
    expect(select.mock.calls).toEqual([["current", "cost"], ["current", "score"], ["current", "writer"]]);
    const reply = Array.from(view.container.querySelectorAll<HTMLButtonElement>(".wt-node[data-trace-kind='reply']")).find((node) => node.textContent?.includes("机会评分的实际证据"));
    expect(reply).toBeTruthy(); fireEvent.click(reply!);
    expect(select).toHaveBeenLastCalledWith("current", "score");
  });

  it("connects the two participating lanes until their temporary discussion closes", () => {
    const { current } = fixture();
    const open = vi.fn();
    const team: Team = { ...current, tunnels: [{ id: "thread", topic: "定价依据", status: "open", participants: ["cost", "score"], messages: [], created_at: at(1), updated_at: at(2) }] };
    const props = { activeTeamId: team.id, profiles, onSelectAgent: vi.fn(), onOpenTunnel: open };
    const view = render(<WorkspaceTrajectory {...props} teams={[team]} />);
    fireEvent.click(screen.getByRole("button", { name: /打开协商 · 成本核算员 ↔ 机会评分员 · 定价依据/ }));
    expect(open).toHaveBeenCalledWith(team.id, "thread");
    view.rerender(<WorkspaceTrajectory {...props} teams={[{ ...team, tunnels: [{ ...team.tunnels![0], status: "closed" }] }]} />);
    expect(screen.queryByLabelText("正在协商的角色")).toBeNull();
    expect(headers(view.container)).toHaveLength(3);
  });

  it("displays the actual invitation symbol on both endpoints, the open channel and the event legend", () => {
    const { current } = fixture();
    const team: Team = { ...current, tunnels: [{
      id: "review", topic: "复算", status: "open", created_at: at(3), updated_at: at(3), participants: ["cost", "score"],
      messages: [{ id: "ask", seq: 1, at: at(3), from: "cost", to: "score", kind: "cowork_invite", text: "请独立复算" }],
    }] };
    const { container } = render(<WorkspaceTrajectory teams={[team]} activeTeamId={team.id} profiles={profiles} onSelectAgent={vi.fn()} />);
    const invite = '[data-action-icon="cowork_invite"]';
    expect(container.querySelectorAll(`.wt-node[data-trace-kind="tunnel"] ${invite}`)).toHaveLength(2);
    expect(container.querySelector(`.wt-tunnel-rail ${invite}`)).toBeTruthy();
    expect(container.querySelector(`.wt-legend ${invite}`)?.parentElement?.parentElement?.textContent).toBe("协作邀请");
    expect(container.querySelector('.wt-legend [data-action-icon="cowork_decline"]')).toBeNull();
  });

  it("preserves the metadata-only admin column while new same-model roles start independently", () => {
    const select = vi.fn();
    const initial: Team = {
      id: "growing", name: "持续展开的任务", goal: "按需安排两个独立核验角色", created_at: at(0), status: "planning",
      agents: [], files: [], handoffs: [], checkpoints: [],
      admin: { session_id: "admin-session", profile_id: "same-model", model_id: "gpt-6.2-astra", family: "gpt", phase: "planning", response: "" },
      events: [{ at: at(1), kind: "admin_started", message: "正在分析任务" }],
    };
    const view = render(<WorkspaceTrajectory teams={[initial]} activeTeamId={initial.id} profiles={profiles} onSelectAgent={select} />);
    expect(headers(view.container)).toHaveLength(1);
    const adminHeader = headers(view.container)[0];
    const adminId = adminHeader.dataset.laneAgent!;
    const adminLaneId = adminHeader.dataset.laneId!;
    const adminColor = adminHeader.style.getPropertyValue("--lc");
    const markers = () => Array.from(view.container.querySelectorAll<HTMLElement>(".wt-marker[data-event-lane]"));
    const markersFor = (laneId: string) => markers().filter((marker) => marker.dataset.eventLane === laneId);
    const adminStart = markersFor(adminLaneId)[0];
    expect(adminStart).toBeTruthy();
    expect(adminColor).not.toBe("");
    const assertAdminPreserved = () => {
      const current = headers(view.container).find((header) => header.dataset.laneAgent === adminId);
      expect(current).toBe(adminHeader);
      expect(current?.dataset.laneId).toBe(adminLaneId);
      expect(current?.style.getPropertyValue("--lc")).toBe(adminColor);
      expect(markersFor(adminLaneId)).toContain(adminStart);
    };

    const pending: Team = {
      ...initial,
      admin: { ...initial.admin!, phase: "delegated", response: "分别核验成本与机会。" },
      agents: [role(initial.id, "cost", "成本核算员"), role(initial.id, "score", "机会评分员")].map((agent) => ({ ...agent, session_id: null, status: "pending" })),
      events: [...initial.events, { at: at(2), kind: "admin_delegated", message: "已安排两个独立角色" }],
    };
    view.rerender(<WorkspaceTrajectory teams={[pending]} activeTeamId={initial.id} profiles={profiles} onSelectAgent={select} />);
    expect(headers(view.container)).toHaveLength(3);
    assertAdminPreserved();
    const workerHeaders = pending.agents.map((agent) => headers(view.container).find((header) => header.dataset.laneAgent === agent.id)!);
    for (const header of workerHeaders) {
      expect(header.dataset.laneTeam).toBe(initial.id);
      expect(header.dataset.status).toBe("pending");
      expect(header.textContent).toContain("待执行");
      expect(header.textContent).toContain("gpt-6.2-astra");
      expect(markersFor(header.dataset.laneId!)).toHaveLength(0);
    }
    const workerColors = workerHeaders.map((header) => header.style.getPropertyValue("--lc"));

    let latest = pending;
    for (const [index, agent] of pending.agents.entries()) {
      latest = {
        ...latest, status: "running",
        agents: latest.agents.map((value) => value.id === agent.id ? { ...value, session_id: `${initial.id}-session-${value.id}`, status: "running" } : value),
        events: [...latest.events, { at: at(3 + index), agent_id: agent.id, kind: "agent_started", message: agent.name }],
      };
      view.rerender(<WorkspaceTrajectory teams={[latest]} activeTeamId={initial.id} profiles={profiles} onSelectAgent={select} />);
      assertAdminPreserved();
      const header = headers(view.container).find((value) => value.dataset.laneAgent === agent.id)!;
      expect(header).toBe(workerHeaders[index]);
      expect(header.style.getPropertyValue("--lc")).toBe(workerColors[index]);
      expect(header.dataset.status).toBe("running");
      const owned = markersFor(header.dataset.laneId!);
      expect(owned).toHaveLength(1);
      expect(owned[0].dataset.eventType).toBe("start");
      const node = owned[0].closest("button")!;
      expect(node.getAttribute("aria-label")).toContain(agent.name);
      expect(node.textContent).toContain(`${agent.name} · 开始`);
      fireEvent.click(node);
      expect(select).toHaveBeenLastCalledWith(initial.id, agent.id);
      expect(markersFor(workerHeaders[1 - index].dataset.laneId!)).toHaveLength(index);
    }
    expect(select.mock.calls).toEqual([[initial.id, "cost"], [initial.id, "score"]]);
    expect(markersFor(adminLaneId).some((marker) => /成本核算员|机会评分员/.test(marker.closest("button")?.textContent || ""))).toBe(false);
  });

  it("draws both dependency edges across role columns into the actual synthesis role", () => {
    const { current, sessions } = fixture();
    const view = render(<WorkspaceTrajectory teams={[current]} activeTeamId={current.id} profiles={profiles} sessions={sessions} onSelectAgent={vi.fn()} />);
    const { lanes } = projectWorkspaceTrace([current], profiles, current.id, sessions);
    const laneId = (agentId: string) => lanes.find((lane) => lane.agent.id === agentId)!.id;
    expect(new Set(lanes.map((lane) => lane.id)).size).toBe(3);
    const edges = Array.from(view.container.querySelectorAll<SVGPathElement>("path[data-edge-from-lane][data-edge-to-lane]"));
    const merge = edges.filter((edge) => edge.dataset.edgeToLane === laneId("writer") && edge.dataset.edgeFromLane !== laneId("writer"));
    expect(new Set(merge.map((edge) => edge.dataset.edgeFromLane))).toEqual(new Set([laneId("cost"), laneId("score")]));
    expect(edges.some((edge) => (edge.dataset.edgeFromLane === laneId("cost") && edge.dataset.edgeToLane === laneId("score")) || (edge.dataset.edgeFromLane === laneId("score") && edge.dataset.edgeToLane === laneId("cost")))).toBe(false);
    expect(merge.every((edge) => !!edge.getAttribute("d"))).toBe(true);
  });

  it("defaults to the active task and exposes other task roles only in all-lanes mode", () => {
    const { current, archive, sessions } = fixture(); const select = vi.fn();
    const view = render(<WorkspaceTrajectory teams={[current, archive]} activeTeamId={current.id} profiles={profiles} sessions={sessions} onSelectAgent={select} />);
    expect(headers(view.container).every((button) => button.dataset.laneTeam === "current")).toBe(true);
    fireEvent.click(screen.getByRole("button", { name: "全部泳道" }));
    expect(headers(view.container)).toHaveLength(4);
    const historical = headers(view.container).find((button) => button.dataset.laneTeam === "archive")!;
    expect(historical.dataset.laneAgent).toBe("cost"); fireEvent.click(historical);
    expect(select).toHaveBeenLastCalledWith("archive", "cost");
    fireEvent.click(screen.getByRole("button", { name: /当前任务|聚焦/ }));
    expect(headers(view.container)).toHaveLength(3);
    view.rerender(<WorkspaceTrajectory teams={[current, archive]} activeTeamId={archive.id} profiles={profiles} sessions={{}} onSelectAgent={select} />);
    expect(headers(view.container)).toHaveLength(1);
    expect(headers(view.container)[0].dataset.laneTeam).toBe("archive");
  });

  it("keeps shared checkpoints in team records rather than inventing a role column", () => {
    const { current, sessions } = fixture(); const select = vi.fn();
    const view = render(<WorkspaceTrajectory teams={[current]} activeTeamId={current.id} profiles={profiles} sessions={sessions} onSelectAgent={select} />);
    expect(headers(view.container)).toHaveLength(3);
    expect(Array.from(view.container.querySelectorAll(".wt-node")).some((node) => node.textContent?.includes("保存对照版本"))).toBe(false);
    fireEvent.click(screen.getByRole("button", { name: /团队记录/ }));
    const records = screen.getByRole("region", { name: "团队记录" });
    const checkpoint = Array.from(records.querySelectorAll("details")).find((record) => record.querySelector("summary")?.textContent?.includes("保存对照版本"));
    expect(checkpoint).toBeTruthy();
    expect(checkpoint?.querySelector("pre")?.textContent).toBe("保存对照版本");
    expect(select).not.toHaveBeenCalled();
    expect(headers(view.container)).toHaveLength(3);
  });

  it("renders legacy role-owned summaries without admin or session identity metadata", () => {
    const { archive } = fixture(); const select = vi.fn();
    archive.events = [{ at: at(1), agent_id: "cost", kind: "tool", message: "read_file" }, { at: at(2), agent_id: "cost", kind: "tool_finished", message: "read_file: completed" }];
    const view = render(<WorkspaceTrajectory teams={[archive]} activeTeamId={archive.id} profiles={profiles} onSelectAgent={select} />);
    expect(headers(view.container)).toHaveLength(1);
    expect(headers(view.container)[0].textContent).toContain("历史核验员");
    const node = Array.from(view.container.querySelectorAll<HTMLButtonElement>(".wt-node")).find((candidate) => candidate.textContent?.includes("read_file"));
    expect(node).toBeTruthy(); fireEvent.click(node!);
    expect(select).toHaveBeenLastCalledWith("archive", "cost");
    expect(view.container.querySelectorAll(".wt-lane-head[data-lane-agent='admin']")).toHaveLength(0);
  });
});
