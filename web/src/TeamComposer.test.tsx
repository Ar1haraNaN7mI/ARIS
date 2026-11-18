import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { TeamComposer } from "./TeamComposer";
import * as api from "./api";
import type { KeiConfig, TeamDraft } from "./teamTypes";
vi.mock("./api", async (original) => ({ ...(await original<typeof import("./api")>()), rpc: vi.fn() }));
afterEach(() => { cleanup(); vi.resetAllMocks(); });
const config: KeiConfig = { base_url: "https://gateway.example/v1", configured: true, profiles: [
  { id: "research", name: "真实模型一", family: "grok", model: "gateway-model-a", protocol: "chat_completions" },
  { id: "delivery", name: "真实模型二", family: "gpt", model: "gateway-model-b", protocol: "responses" },
] };
describe("Editable team templates", () => {
  it("submits independent research agents and explicit fixed models with a dependent delivery agent", async () => {
    const create = vi.fn().mockResolvedValue({});
    render(<TeamComposer config={config} busy={false} onCreate={create} onConfigure={vi.fn()} />);
    fireEvent.change(screen.getByLabelText("团队目标"), { target: { value: "调查与核实行业趋势" } });
    fireEvent.change(screen.getAllByLabelText("负责模型")[0], { target: { value: "delivery" } });
    fireEvent.click(screen.getByRole("button", { name: /创建团队/ }));
    await waitFor(() => expect(create).toHaveBeenCalledOnce());
    const draft: TeamDraft = create.mock.calls[0][0];
    expect(draft.agents.map((a) => a.profile_id)).toEqual(["delivery", "delivery", "research"]);
    expect(draft.agents.map((a) => a.depends_on)).toEqual([[], [], ["agent-1", "agent-2"]]);
    expect(draft.files).toContain("deliverables/report.md");
  });
  it("loads a saved template without replacing its fixed model assignments and saves edits", async () => {
    const template = { id: "custom-1", name: "运营模板", description: "运营复盘", category: "自定义", builtin: false, goal: "评估渠道", files: ["metrics.csv"], agents: [{ id: "analyst", name: "分析员", role: "analyst", profile_id: "delivery", prompt: "分析共享数据", depends_on: [], files: ["report.md"] }] };
    vi.mocked(api.rpc).mockImplementation(async (_token, method) => method === "templates/list" ? { templates: [template] } : { template: { ...template, id: "copy" } });
    render(<TeamComposer token="token" config={config} busy={false} onCreate={vi.fn()} onConfigure={vi.fn()} />);
    await screen.findByRole("option", { name: /运营模板/ });
    fireEvent.change(screen.getByLabelText(/模板库/), { target: { value: "custom-1" } });
    expect((screen.getByLabelText("负责模型") as HTMLSelectElement).value).toBe("delivery");
    expect((screen.getByLabelText("团队目标") as HTMLTextAreaElement).value).toBe("评估渠道");
    fireEvent.click(screen.getByText("保存为可重复使用的模板"));
    fireEvent.click(screen.getByRole("button", { name: "保存为新模板" }));
    await waitFor(() => expect(api.rpc).toHaveBeenCalledWith("token", "templates/save", expect.objectContaining({ template: expect.objectContaining({ agents: expect.arrayContaining([expect.objectContaining({ profile_id: "delivery" })]) }) })));
  });
  it("does not pretend models are available before gateway configuration", () => {
    render(<TeamComposer busy={false} onCreate={vi.fn()} onConfigure={vi.fn()} />);
    expect((screen.getByRole("button", { name: /创建团队/ }) as HTMLButtonElement).disabled).toBe(true);
    expect(screen.queryByRole("option", { name: /deepseek|grok|gemini/i })).toBeNull();
  });
});
