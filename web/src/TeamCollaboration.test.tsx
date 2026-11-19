import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import { TeamDelivery } from "./TeamDelivery";
import { TeamTunnelThread } from "./TeamTunnel";
import { projectWorkspaceTrace } from "./workspaceTrace";
import { rpc } from "./api";
import type { Team, TeamAgent, TeamTunnel } from "./teamTypes";

vi.mock("./api", () => ({ rpc: vi.fn(), errorText: String }));
afterEach(() => { cleanup(); vi.clearAllMocks(); });
const at = "2026-11-19T02:00:00Z";
const actor = (id: string): TeamAgent => ({ id, name: id, profile_id: "same-model", role: "worker", prompt: "", depends_on: [], files: [], status: "completed", session_id: id });
const channel: TeamTunnel = { id: "tunnel-1", topic: "推广角度", participants: ["b","a"], status: "closed", created_at: at, updated_at: at, messages: [
  { id: "q", seq: 1, at, from: "b", to: "a", kind: "proposal", text: "主推统一接口。依据见接入文档。" },
  { id: "r", seq: 2, at, from: "a", to: "b", kind: "accept", text: "我核过文档，同意。", proposal_id: "q" },
], decision: { proposal_id: "q", accepted_by: ["b","a"], text: "主推统一接口。", at } };
const team: Team = { id: "team", name: "研究", created_at: at, status: "completed", goal: "调研与推广", agents: [actor("a"), actor("b"), { ...actor("writer"), delivery: "final" }], files: [], checkpoints: [], handoffs: [], events: [], tunnels: [channel] };

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
