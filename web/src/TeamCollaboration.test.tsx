import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import { TeamDelivery } from "./TeamDelivery";
import { TeamTunnelThread } from "./TeamTunnel";
import { ProcessChat } from "./ProcessChat";
import { tunnelRequestState } from "./tunnelMessages";
import { actionLabels, messageAction } from "./collaborationActions";
import { projectWorkspaceTrace } from "./workspaceTrace";
import { rpc } from "./api";
import type { Team, TeamAgent, TeamTunnel, TunnelMessage } from "./teamTypes";
import type { SessionResult } from "./types";

vi.mock("./api", () => ({ rpc: vi.fn(), errorText: String }));
afterEach(() => { cleanup(); vi.clearAllMocks(); });
const at = "2026-11-19T02:00:00Z";
const actor = (id: string): TeamAgent => ({ id, name: id, profile_id: "same-model", role: "worker", prompt: "", depends_on: [], files: [], status: "completed", session_id: id });
const channel: TeamTunnel = { id: "tunnel-1", topic: "推广角度", participants: ["b","a"], status: "closed", created_at: at, updated_at: at, messages: [
  { id: "q", seq: 1, at, from: "b", to: "a", kind: "proposal", text: "主推统一接口。依据见接入文档。" },
  { id: "r", seq: 2, at, from: "a", to: "b", kind: "accept", text: "我核过文档，同意。", proposal_id: "q" },
], decision: { proposal_id: "q", accepted_by: ["b","a"], text: "主推统一接口。", at } };
const team: Team = { id: "team", name: "研究", created_at: at, status: "completed", goal: "调研与推广", agents: [actor("a"), actor("b"), { ...actor("writer"), delivery: "final" }], files: [], checkpoints: [], handoffs: [], events: [], tunnels: [channel] };

it("uses a distinct outline and a readable action label for each peer action in both lanes and the conversation", () => {
  const kinds: TunnelMessage["kind"][] = ["message", "question", "answer", "notice", "proposal", "accept", "cowork_invite", "cowork_accept", "cowork_decline", "cowork_result"];
  const messages = kinds.map((kind, i) => ({ ...channel.messages[0], id: String(i), seq: i, kind }));
  const tunnel = { ...channel, messages, decision: null };
  const sample = { ...team, tunnels: [tunnel] };
  const view = render(<TeamTunnelThread team={sample} tunnel={tunnel} onSelectActor={vi.fn()} onSelectTunnel={vi.fn()} />);
  const paths = new Set<string>();
  for (const message of messages) {
    const heading = view.container.querySelector(`[data-peer-kind="${message.kind}"] .lp-tunnel-byline strong`)!;
    const icon = heading.querySelector(`svg[data-action-icon="${message.kind}"]`)!;
    expect(heading.textContent).toBe(actionLabels[message.kind]);
    expect(icon.getAttribute("aria-hidden")).toBe("true");
    paths.add(icon.querySelector("path")!.getAttribute("d")!);
  }
  expect(paths.size).toBe(kinds.length);
  for (const lane of projectWorkspaceTrace([sample]).lanes.filter((l) => tunnel.participants.includes(l.agent.id))) {
    expect(lane.nodes.map((node) => node.action)).toEqual(kinds);
  }
  const legacy = messages[0];
  expect(messageAction({ ...legacy, expects_reply: true })).toBe("question");
  expect(messageAction({ ...legacy, expects_reply: false })).toBe("notice");
  expect(messageAction({ ...legacy, reply_to: "earlier" })).toBe("answer");
});

it("connects each tunnel message from its actual sender to its peer, including reverse messages at the same timestamp", () => {
  const trace = projectWorkspaceTrace([team]);
  const nodes = trace.lanes.flatMap((lane) => lane.nodes);
  expect(nodes.filter(n => n.tunnelId === channel.id)).toHaveLength(4);
  const hops = nodes.filter(n => n.sentNodeId).map((node) => {
    const sender = nodes.find(n => n.id === node.sentNodeId)!;
    expect(node.parentKinds[sender.id]).toBe("tunnel");
    expect(node.y).toBeGreaterThan(sender.y);
    return [sender.agentId, node.agentId, sender.detail];
  });
  expect(hops).toContainEqual(["b", "a", channel.messages[0].text]);
  expect(hops).toContainEqual(["a", "b", channel.messages[1].text]);
});

it("keeps both speakers and their agreed wording visible after the temporary lane closes", () => {
  const select = vi.fn();
  const page = render(<TeamTunnelThread team={team} tunnel={channel} onSelectActor={select} onSelectTunnel={vi.fn()} />);
  for (const message of channel.messages) expect(screen.getByText(message.text)).toBeTruthy();
  expect(screen.getByLabelText("双方确认的结论").textContent).toContain("主推统一接口。");
  fireEvent.click(page.container.querySelector('[data-tunnel-from="a"] button')!);
  expect(select).toHaveBeenCalledWith("a");
});

it("shows independent co-work turns and the return to the assignment in the actual worker's chat and lane", () => {
  const sample = { ...team, events: [
    { at, agent_id: "b", kind: "cowork_turn_started", message: "work on the invitation" },
    { at, agent_id: "b", kind: "cowork_turn_finished", message: "return to pending assignment" },
  ] };
  const props = { team: sample, sessions: {}, profiles: [], onSelectAgent: vi.fn() };
  const view = render(<ProcessChat {...props} agentId="b" />);
  for (const kind of ["cowork_turn_started", "cowork_turn_finished"] as const) {
    expect(view.container.querySelector(`.lp-note svg[data-action-icon="${kind}"]`)?.parentElement?.textContent).toContain(actionLabels[kind]);
  }
  const trace = projectWorkspaceTrace([sample]).lanes.find((lane) => lane.agent.id === "b")!;
  expect(trace.nodes.filter((node) => node.source === "team").map((node) => node.action)).toEqual(["cowork_turn_started", "cowork_turn_finished"]);
  view.rerender(<ProcessChat {...props} agentId="a" />);
  expect(view.container.querySelector('.lp-note svg[data-action-icon="cowork_turn_started"]')).toBeNull();
});

it("shows one copy of a peer exchange in either participant's chat, including co-work results and passive notices", () => {
  function message(seq: number, kind: TunnelMessage["kind"], text: string, from: string, reply_to?: string): TunnelMessage {
    return { id: String(seq), seq, from, to: from === "a" ? "b" : "a", at, kind, text, reply_to };
  }
  const work: TeamTunnel = { ...channel, decision: null, messages: [
    message(1, "cowork_invite", "请复算容量表", "a"), message(2, "cowork_accept", "我来核算", "b", "1"),
    message(3, "cowork_result", "容量上限为 16 席", "b", "1"), message(4, "notice", "已采纳 16 席上限", "a"),
    message(5, "question", "需要预留客服时间吗？", "a"), message(6, "answer", "每天预留两小时", "b", "5"),
  ] };
  const props = { team: { ...team, tunnels: [work] }, sessions: {}, profiles: [], onSelectAgent: vi.fn(), onOpenTunnel: vi.fn() };
  const page = render(<ProcessChat {...props} agentId="a" />);
  for (const agentId of ["a", "b", undefined]) {
    page.rerender(<ProcessChat {...props} agentId={agentId} />);
    expect(page.container.querySelectorAll("[data-peer-kind]")).toHaveLength(6);
    for (const record of work.messages) expect(screen.getAllByText(record.text, { selector: "p" })).toHaveLength(1);
    const notification = within(page.container.querySelector<HTMLElement>('[data-peer-kind="notice"]')!);
    expect(notification.getByText("通知")).toBeTruthy();
    expect(notification.queryByText("待答复")).toBeNull();
    expect(screen.getByText("已交付")).toBeTruthy();
    expect(screen.getByText("已答复")).toBeTruthy();
    fireEvent.click(notification.getByRole("button", { name: /打开协作记录/ }));
    expect(props.onOpenTunnel).toHaveBeenCalledWith(work.id);
  }
  page.rerender(<ProcessChat {...props} agentId="writer" />);
  expect(page.container.querySelector("[data-peer-kind]")).toBeNull();
  const trace = projectWorkspaceTrace([props.team]).lanes.flatMap((lane) => lane.nodes);
  expect(trace.filter((node) => node.detail === "已采纳 16 席上限").map((node) => node.label).sort()).toEqual(["通知 → b", "通知 ← a"].sort());
  expect(trace.filter((node) => node.detail === "容量上限为 16 席").map((node) => node.label).sort()).toEqual(["交付协作成果 → a", "交付协作成果 ← b"].sort());

  const [invite, accept] = work.messages;
  const open: TeamTunnel = { ...work, status: "open", messages: [invite, accept] };
  expect(tunnelRequestState(open, invite)).toBe("已接手");
  const skippedInvite = { ...invite, followup_closed_reason: "follow-up budget exhausted" };
  const question = work.messages[4];
  const partial: TeamTunnel = { ...open, messages: [skippedInvite, accept, question] };
  expect(tunnelRequestState(partial, skippedInvite)).toBe("未交付");
  expect(tunnelRequestState(partial, question)).toBe("待答复");
  expect(tunnelRequestState(partial, { ...question, followup_closed_reason: "budget exhausted" })).toBe("未答复");
  expect(tunnelRequestState({ ...open, status: "interrupted" }, invite)).toBe("未交付");
  expect(tunnelRequestState({ ...open, messages: [invite, { ...accept, kind: "cowork_decline" }] }, invite)).toBe("已婉拒");
});

it("uses the shared peer record in place of the matching recipient note, keeping history without a shared record", () => {
  const note = { id: "tunnel:tunnel-1:q", type: "system_note", text: "原始会话的协作通知", at };
  const sessions = { a: { session: { id: "a", name: null, events: [note] }, task_id: null, state: null } } satisfies Record<string, SessionResult>;
  const props = { team, sessions, profiles: [], agentId: "a", onSelectAgent: vi.fn() };
  const view = render(<ProcessChat {...props} />);
  expect(screen.queryByText(note.text)).toBeNull();
  const notes = (current: Team) => projectWorkspaceTrace([current], [], current.id, sessions).lanes.flatMap((lane) => lane.nodes).filter((node) => node.eventId === note.id);
  expect(notes(team)).toHaveLength(0);
  const legacy = { ...team, tunnels: [] };
  view.rerender(<ProcessChat {...props} team={legacy} />);
  expect(screen.getByText(note.text)).toBeTruthy();
  expect(notes(legacy)).toHaveLength(1);
});

it("shows the shipped draft and its supporting research from the final run, with access to the source conversation", async () => {
  const file = (path: string, text: string) => ({path, text, sha256: "snapshot", size_bytes: 20});
  vi.mocked(rpc).mockResolvedValue({ delivery: { agent_id:"writer", run_id:"final-run", status:"completed", stale:false, text:"已写好", artifacts:[file("推广.md", "可发布正文")], sources:[{agent_id:"a", artifacts:[file("调研.md", "原始来源报告")]}], tunnel_ids:[channel.id] } });
  const open = vi.fn();
  render(<TeamDelivery team={team} token="local" onOpenTunnel={open} />);
  await waitFor(() => expect(screen.getByText("可发布正文")).toBeTruthy());
  expect(screen.queryByText("已写好")).toBeNull();
  fireEvent.click(screen.getByRole("button", {name:"调研.md"}));
  expect(screen.getByText("原始来源报告")).toBeTruthy();
  fireEvent.click(screen.getByRole("button", {name:/查看协商/}));
  expect(open).toHaveBeenCalledWith(channel.id);
});

it("keeps a final chat draft ahead of the supporting files", async () => {
  vi.mocked(rpc).mockResolvedValue({ delivery: {
    agent_id: "writer", status: "completed", stale: false, text: "直接发布的文字", artifacts: [],
    sources: [{ agent_id: "a", artifacts: [{ path: "sources.md", text: "| 来源 | 用途 |\n| --- | --- |\n| 原始来源 | 核验 |" }] }], tunnel_ids: [],
  } });
  render(<TeamDelivery team={team} token="local" onOpenTunnel={vi.fn()} />);
  await screen.findByText("直接发布的文字");
  fireEvent.click(screen.getByRole("button", { name: "sources.md" }));
  await screen.findByText("原始来源");
  expect(screen.getByRole("table")).toBeTruthy();
  expect(screen.getByText("a · 研究依据")).toBeTruthy();
  fireEvent.click(screen.getByRole("button", { name: "最终稿" }));
  await screen.findByText("直接发布的文字");
});
