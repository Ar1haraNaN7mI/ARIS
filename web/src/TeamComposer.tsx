import { useEffect, useState, type FormEvent } from "react";
import { ArrowRightIcon, PlusIcon, TrashIcon } from "@radix-ui/react-icons";
import type { AgentDraft, KeiConfig, TeamDraft, TeamTemplate } from "./teamTypes";
import { errorText, rpc } from "./api";

const presets = {
  research: { label: "调研", note: "并行查证，汇总结论", agents: [
    { name: "调研员", role: "researcher", prompt: "调研目标主题，记录事实、来源链接与不确定项。将研究材料写入指定交付文件。", depends_on: [] },
    { name: "独立核验", role: "reviewer", prompt: "独立调查目标，核查关键假设，提供证据与不同观点。将核验材料写入指定交付文件。", depends_on: [] },
    { name: "汇总交付", role: "writer", prompt: "阅读两位研究员的研究产物，解决分歧，生成有来源的最终报告。", depends_on: ["agent-1", "agent-2"] },
  ], files: ["research.md", "verification.md", "report.md"] },
  writing: { label: "写作", note: "资料 → 草稿 → 编辑", agents: [
    { name: "资料整理", role: "researcher", prompt: "梳理目标、受众和资料，建立事实依据与写作提纲。", depends_on: [] },
    { name: "内容创作", role: "writer", prompt: "读取上游资料，围绕目标完成清晰完整的草稿。", depends_on: ["agent-1"] },
    { name: "编辑交付", role: "editor", prompt: "审阅草稿的事实、结构和表达，完成最终稿并说明修改。", depends_on: ["agent-2"] },
  ], files: ["outline.md", "draft.md", "final.md"] },
  build: { label: "构建", note: "分析 → 实现 → 验证", agents: [
    { name: "需求分析", role: "planner", prompt: "检查已有上下文与依赖，明确实现方案、交付文件和验收方法。", depends_on: [] },
    { name: "执行实现", role: "builder", prompt: "依据分析实现目标，复用现有能力，记录改动与运行方法。", depends_on: ["agent-1"] },
    { name: "独立验证", role: "reviewer", prompt: "检查实现和实际结果，运行必要验证，交付结果与剩余问题。", depends_on: ["agent-2"] },
  ], files: ["plan.md", "implementation.md", "validation.md"] },
};
type Preset = keyof typeof presets;
function draftAgents(preset: Preset, config?: KeiConfig): AgentDraft[] {
  const definition = presets[preset];
  const available = config?.profiles.filter((p) => p.model.trim()) || [];
  const finalRole = { research: "writer", writing: "editor", build: undefined }[preset];
  return definition.agents.map((agent, i) => ({
    ...agent, id: `agent-${i + 1}`, profile_id: available[i % (available.length || 1)]?.id || "",
    files: [`deliverables/${definition.files[i]}`], delivery: agent.role === finalRole ? "final" : undefined,
  }));
}
export function TeamComposer({ token, config, busy, onCreate, onConfigure, initialGoal = "", initialTemplate, initialDraft, onDraftChange }: {
  token?: string; config?: KeiConfig; busy: boolean; onCreate: (draft: TeamDraft) => Promise<unknown>; onConfigure: () => void;
  initialGoal?: string; initialTemplate?: TeamTemplate; initialDraft?: TeamDraft; onDraftChange?: (draft: TeamDraft) => void;
}) {
  const seed = initialDraft || initialTemplate;
  const [preset, setPreset] = useState<Preset>("research");
  const [name, setName] = useState(seed?.name || "");
  const [goal, setGoal] = useState(initialDraft?.goal || initialGoal || initialTemplate?.goal || "");
  const [agents, setAgents] = useState<AgentDraft[]>(() => seed?.agents.map((agent) => ({ ...agent, files: [...agent.files], depends_on: [...agent.depends_on] })) || draftAgents("research", config));
  const [shared, setShared] = useState(seed?.files.join("\n") || "");
  const [templates, setTemplates] = useState<TeamTemplate[]>([]);
  const [templateId, setTemplateId] = useState(initialTemplate?.id || "");
  const [templateName, setTemplateName] = useState("");
  const [templateDescription, setTemplateDescription] = useState("");
  const [templateBusy, setTemplateBusy] = useState(false);
  const [templateError, setTemplateError] = useState("");
  const [templateNotice, setTemplateNotice] = useState("");
  const profiles = config?.profiles.filter((p) => p.model.trim()) || [];
  const finalChoices = agents.filter((agent) => !agents.some((peer) => peer.depends_on.includes(agent.id)));
  const validAssignments = agents.every((agent, index) => profiles.some((profile) => profile.id === (agent.profile_id || profiles[index % (profiles.length || 1)]?.id)));
  useEffect(() => {
    if (!token) return;
    const abort = new AbortController();
    void rpc<{ templates: TeamTemplate[] }>(token, "templates/list", {}, abort.signal).then((result) => { if (!abort.signal.aborted) setTemplates(result.templates); }).catch((e) => { if (!abort.signal.aborted) setTemplateError(errorText(e)); });
    return () => abort.abort();
  }, [token]);
  function update(index: number, patch: Partial<AgentDraft>) {
    setAgents((items) => items.map((agent, i) => {
      if (i === index) return { ...agent, ...patch };
      if (agent.delivery === "final" && patch.depends_on?.includes(agent.id)) return { ...agent, delivery: undefined };
      return agent;
    }));
  }
  function currentDraft(): TeamDraft {
    const resolved = agents.map((agent, i) => ({ ...agent, profile_id: agent.profile_id || profiles[i % (profiles.length || 1)]?.id || "" }));
    return { name: name.trim() || goal.trim().slice(0, 36), goal: goal.trim(), agents: resolved, files: [...new Set([...shared.split(/\r?\n/).map((s) => s.trim()).filter(Boolean), ...agents.flatMap((a) => a.files)])] };
  }
  useEffect(() => { onDraftChange?.(currentDraft()); }, [name, goal, agents, shared, config]);
  async function submit(event: FormEvent) {
    event.preventDefault(); await onCreate(currentDraft());
  }
  function loadTemplate(id: string) {
    const template = templates.find((item) => item.id === id);
    setTemplateId(id);
    if (!template) return;
    setAgents(template.agents.map((agent) => ({ ...agent, depends_on: [...agent.depends_on], files: [...agent.files] })));
    setGoal(template.goal); setName(template.name); setShared(template.files.join("\n"));
    setTemplateName(`${template.name} · 自定义`); setTemplateDescription(template.description);
    setTemplateNotice(""); setTemplateError("");
  }
  async function saveTemplate() {
    if (!token || templateBusy) return;
    setTemplateBusy(true); setTemplateError(""); setTemplateNotice("");
    try {
      const result = await rpc<{ template: TeamTemplate }>(token, "templates/save", { template: { ...currentDraft(), name: templateName.trim() || name.trim() || "我的团队模板", description: templateDescription.trim(), category: "自定义" } });
      setTemplates((items) => [...items.filter((item) => item.id !== result.template.id), result.template]); setTemplateId(result.template.id); setTemplateNotice("模板已保存，智能体的职责、模型、依赖和文件均会保留。");
    } catch (e) { setTemplateError(errorText(e)); } finally { setTemplateBusy(false); }
  }
  return <form className="team-composer" onSubmit={(event) => void submit(event)}>
    <div className="aris-section-heading"><span className="eyebrow">BUILD YOUR TEAM</span><h1>一个目标，多种思考。</h1><p>让不同模型分工、共享文件，根据依赖自动继续完成任务。</p></div>
    <div className="team-presets" aria-label="工作流预设">{Object.entries(presets).map(([id, item]) => <button type="button" key={id} aria-pressed={!templateId && preset === id} onClick={() => { setPreset(id as Preset); setTemplateId(""); setAgents(draftAgents(id as Preset, config)); }}><strong>{item.label}</strong><span>{item.note}</span></button>)}</div>
    <div className="template-library"><label className="aris-field">模板库 <small>载入后可以自由修改每位智能体</small><select value={templateId} onChange={(e) => loadTemplate(e.target.value)}><option value="">选择实用模板或已保存的团队…</option>{[...new Set(templates.map((t) => t.category))].map((category) => <optgroup key={category} label={category}>{templates.filter((t) => t.category === category).map((t) => <option key={t.id} value={t.id}>{t.name} · {t.agents.length} 位智能体</option>)}</optgroup>)}</select></label>{templates.find((t) => t.id === templateId)?.description && <p className="subtle">{templates.find((t) => t.id === templateId)?.description}</p>}</div>
    {templateError && <div className="aris-error" role="alert">{templateError}</div>}
    {templateNotice && <p className="aris-success" role="status">{templateNotice}</p>}
    <label className="aris-field">任务名称<input value={name} onChange={(e) => setName(e.target.value)} placeholder="例如：新产品发布调研" /></label>
    <label className="aris-field">团队目标<textarea required rows={3} value={goal} onChange={(e) => setGoal(e.target.value)} placeholder="描述你要完成的事、已有资料，以及怎样才算完成…" /></label>
    <div className="aris-split-heading"><h2>分工与依赖</h2><span className="subtle">最多 32 位智能体 · 每批最多 8 位并行</span></div>
    {!profiles.length && <div className="aris-info">先配置 KEI API 模型档案，再为团队分配模型。<button type="button" onClick={onConfigure}>配置模型 <ArrowRightIcon /></button></div>}
    {profiles.length > 0 && !validAssignments && <div className="aris-info">模板中有尚未配置的模型档案，请为每位智能体选择可用模型，或先补充模型连接。</div>}
    <div className="agent-drafts">{agents.map((agent, index) => <fieldset className="agent-draft" key={agent.id}>
      <legend><span>{String(index + 1).padStart(2, "0")}</span>{agent.name}</legend>
      <div className="agent-draft-top"><label className="aris-field">名称<input required value={agent.name} onChange={(e) => update(index, { name: e.target.value })} /></label><label className="aris-field">负责模型<select required value={agent.profile_id || profiles[index % (profiles.length || 1)]?.id || ""} onChange={(e) => update(index, { profile_id: e.target.value })}><option value="" disabled>选择模型档案</option>{agent.profile_id && !config?.profiles.some((p) => p.id === agent.profile_id) && <option value={agent.profile_id} disabled>{agent.profile_id} · 档案已移除</option>}{(config?.profiles || []).map((p) => <option key={p.id} value={p.id} disabled={!p.model.trim()}>{p.name} · {p.model || "尚未配置模型 ID"}</option>)}</select></label><button className="icon-button" type="button" disabled={agents.length <= 1} aria-label={`移除 ${agent.name}`} onClick={() => setAgents((items) => items.filter((a) => a.id !== agent.id).map((a) => ({ ...a, depends_on: a.depends_on.filter((id) => id !== agent.id) })))}><TrashIcon /></button></div>
      <label className="aris-field">职责与交付要求<textarea required rows={2} value={agent.prompt} onChange={(e) => update(index, { prompt: e.target.value })} /></label>
      <label className="aris-field">角色标识<input required value={agent.role} onChange={(e) => update(index, { role: e.target.value })} placeholder="researcher / writer / reviewer / 自定义角色" /></label>
      <label className="aris-field">交付文件 <small>相对工作区路径，每行一个</small><textarea rows={1} value={agent.files.join("\n")} onChange={(e) => update(index, { files: e.target.value.split(/\r?\n/).map((s) => s.trim()).filter(Boolean) })} /></label>
      <div className="agent-dependencies"><span>前置子流程</span>{agents.filter((candidate) => candidate.id !== agent.id).map((candidate) => <label key={candidate.id}><input type="checkbox" checked={agent.depends_on.includes(candidate.id)} onChange={(e) => update(index, { depends_on: e.target.checked ? [...agent.depends_on, candidate.id] : agent.depends_on.filter((id) => id !== candidate.id) })} />{candidate.name}</label>)}{!agent.depends_on.length && <small>独立开始</small>}</div>
    </fieldset>)}</div>
    <button type="button" className="outline-button" disabled={agents.length >= 32} onClick={() => setAgents((items) => [...items, { id: `agent-${crypto.randomUUID().slice(0, 8)}`, name: `智能体 ${items.length + 1}`, role: "assistant", profile_id: profiles[0]?.id || "", prompt: "", depends_on: [], files: [] }])}><PlusIcon />添加智能体</button>
    <label className="aris-field">最终汇总<select value={agents.find((actor) => actor.delivery === "final")?.id || ""}
      onChange={(event) => { const id = event.target.value; setAgents((items) => items.map((actor) => ({
        ...actor, delivery: actor.id === id ? "final" : actor.delivery === "final" ? undefined : actor.delivery,
      }))); }}>
      <option value="">各自交付</option>{finalChoices.map((actor) => <option key={actor.id} value={actor.id}>{actor.name}</option>)}
    </select></label>
    <label className="aris-field shared-draft">共享输入文件 <small>相对工作区路径，每行一个</small><textarea rows={2} value={shared} onChange={(e) => setShared(e.target.value)} placeholder="brief.md" /></label>
    <details className="template-save"><summary>保存为可重复使用的模板</summary><label className="aris-field">模板名称<input value={templateName} onChange={(e) => setTemplateName(e.target.value)} placeholder={name || "我的团队模板"} /></label><label className="aris-field">模板说明<input value={templateDescription} onChange={(e) => setTemplateDescription(e.target.value)} placeholder="适合完成什么工作？" /></label><div className="inline-actions"><button type="button" className="outline-button" disabled={templateBusy || !profiles.length || !goal.trim() || agents.some((a) => !a.prompt.trim())} onClick={() => void saveTemplate()}>保存为新模板</button>{templateId && templates.find((t) => t.id === templateId)?.builtin === false && <button type="button" className="outline-button" disabled={templateBusy} onClick={() => { setTemplateBusy(true); void rpc(token!, "templates/delete", { id: templateId }).then(() => { setTemplates((items) => items.filter((t) => t.id !== templateId)); setTemplateId(""); setTemplateNotice("模板已删除，当前草稿保留。"); }).catch((e) => setTemplateError(errorText(e))).finally(() => setTemplateBusy(false)); }}>删除此模板</button>}</div></details>
    <div className="composer-submit"><span>团队与执行记录保存在本机</span><button className="solid-button" disabled={busy || !goal.trim() || !profiles.length || !validAssignments}>{busy ? "正在创建…" : "创建团队"}<ArrowRightIcon /></button></div>
  </form>;
}
