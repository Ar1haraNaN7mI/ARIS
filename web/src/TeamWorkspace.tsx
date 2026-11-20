import { lazy, Suspense, useEffect, useState, type CSSProperties } from "react";
import * as Dialog from "@radix-ui/react-dialog";
import { ChatBubbleIcon, Cross1Icon, FileTextIcon, GearIcon, HamburgerMenuIcon, MagnifyingGlassIcon, PlusIcon, ReloadIcon, StopIcon } from "@radix-ui/react-icons";
import { rpc } from "./api";
import { useHarness } from "./useHarness";
import { useTeams } from "./useTeams";
import { useTeamActivity } from "./useTeamActivity";
import { SettingsCenter } from "./SettingsCenter";
import { TeamComposer } from "./TeamComposer";
import { KeiSettings } from "./KeiSettings";
import { ComputerSettings } from "./ComputerSettings";
import { TeamDetails } from "./TeamDetails";
import { TeamTimeline, MessageComposer } from "./TeamConversation";
import { ProcessChat } from "./ProcessChat";
import { TeamTunnelThread } from "./TeamTunnel";
import { ModelGlyph, ModelReel, ProcessRelay } from "./ProcessRelay";
import { WorkspaceTrajectory } from "./WorkspaceTrajectory";
import { WorkflowReview } from "./WorkflowReview";
import { Emblem } from "./Emblem";
import { type TeamTemplate, type TeamDraft, type ModelProfile } from "./teamTypes";
import { ADMIN_VIEW, adminAgent, conversationAgents, isAdminOnly, progressAgents, teamIsActive, workerAgents } from "./teamConversationModel";
import type { Bootstrap, Profile } from "./types";
import "./aris.css";
import "./workspace.css";

const TeamDelivery = lazy(() => import("./TeamDelivery").then(({ TeamDelivery }) => ({ default: TeamDelivery })));

export function templateDraft(template: TeamTemplate, goal: string, profiles: ModelProfile[], selected?: string): TeamDraft {
  const fallback = profiles.find((profile) => profile.id === selected && profile.model) || profiles.find((profile) => profile.model);
  return { name: goal.trim().slice(0, 36) || template.name, goal, files: [...template.files], agents: template.agents.map((agent) => ({ ...agent, depends_on: [...agent.depends_on], files: [...agent.files], profile_id: profiles.find((profile) => profile.id === agent.profile_id && profile.model)?.id || fallback?.id || agent.profile_id })) };
}

export function TeamWorkspace({ initialData, latestProfile, onChat }: { initialData?: Bootstrap; latestProfile?: Profile; onChat: () => void }) {
  const harness = useHarness(initialData, latestProfile, false);
  const { data } = harness;
  const state = useTeams(data?.token);
  const { team, config } = state;
  const [settings, setSettings] = useState(false);
  const [mode, setMode] = useState<"home" | "team" | "models">("home");
  const [selection, setSelection] = useState("overview");
  const [view, setView] = useState<"chat" | "timeline" | "browser" | "tunnel" | "delivery">("chat");
  const [tunnelId, setTunnelId] = useState("");
  const [mobileNav, setMobileNav] = useState(false);
  const [details, setDetails] = useState(false);
  const [createOpen, setCreateOpen] = useState(false);
  const [draftVersion, setDraftVersion] = useState(0);
  const [quickGoal, setQuickGoal] = useState("");
  const [initialTemplate, setInitialTemplate] = useState<TeamTemplate>();
  const [savedDraft, setSavedDraft] = useState<TeamDraft>();
  const [templates, setTemplates] = useState<TeamTemplate[]>([]);
  const [templateId, setTemplateId] = useState("");
  const [newProfile, setNewProfile] = useState("");
  const [adminProfiles, setAdminProfiles] = useState<Record<string, string>>({});
  const [localError, setLocalError] = useState("");
  const [search, setSearch] = useState("");
  const activeTeam = mode === "team" ? team : undefined;
  const activity = useTeamActivity(data?.token, activeTeam);
  const admin = activeTeam ? adminAgent(activeTeam) : undefined;
  const workers = activeTeam ? workerAgents(activeTeam) : [];
  const agent = workers.find((item) => item.id === selection);
  const selectedAdmin = !!admin && (selection === ADMIN_VIEW || selection === admin.id);
  const conversation = selectedAdmin ? admin : agent;
  const channels = activeTeam?.tunnels || [];
  const focusedTunnel = channels.find((item) => item.id === tunnelId);
  const relatedChannels = agent ? channels.filter((item) => item.participants.includes(agent.id)) : channels;
  const finalAuthor = workers.find((item) => item.delivery === "final");
  const simple = isAdminOnly(activeTeam);
  const running = teamIsActive(activeTeam);
  const adminBusy = activeTeam?.admin?.phase === "planning";
  const profiles = config?.profiles.filter((item) => item.model.trim()) || [];
  const profile = config?.profiles.find((item) => item.id === conversation?.profile_id);
  const reelAgent = agent || admin || activeTeam?.agents.find((item) => item.status === "running") || activeTeam?.agents[0];
  const reelProfile = mode === "team" ? (!agent && activeTeam && adminProfiles[activeTeam.id]) || reelAgent?.profile_id : newProfile;
  const selectedTemplate = templates.find((item) => item.id === templateId);
  useEffect(() => {
    if (!data?.token) return;
    const abort = new AbortController();
    void rpc<{ templates: TeamTemplate[] }>(data.token, "templates/list", {}, abort.signal).then((result) => { if (!abort.signal.aborted) setTemplates(result.templates); }).catch(() => { if (!abort.signal.aborted) setLocalError("模板暂时无法读取，可以直接发送消息。"); });
    return () => abort.abort();
  }, [data?.token, createOpen]);
  useEffect(() => {
    const desktop = window as Window & { __ARIS_DESKTOP__?: boolean; ipc?: { postMessage?: (message: string) => void } };
    if (data && desktop.__ARIS_DESKTOP__) desktop.ipc?.postMessage?.(JSON.stringify({ event: "aris-ready", version: 1 }));
  }, [data]);
  function selectTeam(id: string, agentId = "overview") { state.select(id); setMode("team"); setSelection(agentId); setView("chat"); setMobileNav(false); }
  function selectAgent(id: string) { setSelection(id); setView("chat"); setMobileNav(false); }
  function openTunnel(id: string, channel: string) {
    if (activeTeam?.id !== id) selectTeam(id);
    setTunnelId(channel); setView("tunnel"); setMobileNav(false);
  }
  function newChat() { setMode("home"); setSelection("overview"); setView("chat"); setMobileNav(false); setLocalError(""); }
  function startDraft(template?: TeamTemplate) { setSavedDraft(undefined); setInitialTemplate(template); setDraftVersion((value) => value + 1); setCreateOpen(true); setMobileNav(false); }
  async function sendNewGoal(goal: string) {
    setLocalError("");
    if (!profiles.length) { setLocalError("请先连接一个模型，再发送消息。"); setMode("models"); return; }
    const created = selectedTemplate ? await state.create(templateDraft(selectedTemplate, goal, profiles, newProfile)) : await state.admin(goal, undefined, newProfile || undefined);
    if (!created) return;
    selectTeam(created.id, !selectedTemplate ? adminAgent(created)?.id : undefined); setQuickGoal("");
    if (selectedTemplate) await state.run(created.id);
    return created;
  }
  function resumeTeam() {
    if (!activeTeam) return;
    if (activeTeam.admin && ["failed", "cancelled"].includes(activeTeam.admin.phase)) {
      void state.admin("继续完成上一次未完成的请求。", activeTeam.id, adminProfiles[activeTeam.id]);
    } else {
      void state.run(activeTeam.id);
    }
  }
  function changeModel(id: string) {
    if (!id) { if (!profiles.length) setMode("models"); else setNewProfile(""); return; }
    if (activeTeam && reelAgent) {
      if (running) { setLocalError("本轮执行完成后可切换模型。"); return; }
      if (admin && !agent) setAdminProfiles((current) => ({ ...current, [activeTeam.id]: id }));
      else void state.model(activeTeam.id, reelAgent.id, id);
    } else setNewProfile(id);
  }
  const taskList = state.teams.filter((item) => `${item.name} ${item.goal} ${item.agents.map((member) => member.name).join(" ")}`.toLowerCase().includes(search.toLowerCase()));
  return <div className={`aris-shell aw-shell lp-shell ${mobileNav ? "aw-nav-open" : ""} ${mode === "models" ? "lp-settings-mode" : ""}`}>
    <header className="aw-topbar"><button className="aw-icon aw-nav-toggle" aria-label="切换任务侧栏" onClick={() => setMobileNav((value) => !value)}><HamburgerMenuIcon /></button><button className="aw-brand" onClick={newChat}><Emblem size={22} /><strong>ARIS</strong><small>多任务 · 会话过程</small></button><ModelReel allowAuto={mode !== "team"} profiles={profiles} selected={reelProfile} onSelect={changeModel} /><div className="aw-top-tools">{running && activeTeam ? <button className="aw-button" disabled={state.busy || activeTeam.status === "cancelling"} onClick={() => void state.cancel(activeTeam.id)}><StopIcon />{activeTeam.status === "cancelling" ? "停止中" : "停止"}</button> : activeTeam && activeTeam.status !== "completed" && <button className="aw-button primary" disabled={state.busy} onClick={resumeTeam}>▶ 继续</button>}<button className="aw-button" onClick={() => setMode("models")}><span className={`aw-connection ${config?.configured ? "ready" : ""}`} />KEI API</button><SettingsCenter harness={harness} open={settings} onOpenChange={(open) => { setSettings(open); if (!open) void state.refreshConfig(); }} /></div></header>
    {mobileNav && <button className="aw-nav-scrim" aria-label="关闭任务侧栏" onClick={() => setMobileNav(false)} />}
    <aside className="aw-sidebar"><div className="aw-sidebar-top"><label className="aw-search"><MagnifyingGlassIcon /><input aria-label="搜索任务" placeholder="搜索会话" value={search} onChange={(event) => setSearch(event.target.value)} /></label><button className="aw-icon lp-new-chat" aria-label="新建会话" title="新建会话" onClick={newChat}><PlusIcon /></button></div><nav className="aw-sidebar-scroll" aria-label="任务与子流程">{taskList.map((item, index) => <section className="lp-session-group" key={item.id}><button className={`lp-group-heading ${activeTeam?.id === item.id && selection === "overview" ? "selected" : ""}`} onClick={() => selectTeam(item.id)}><span>{item.name}</span><small>{isAdminOnly(item) ? item.status === "planning" ? "处理中" : "" : `${progressAgents(item).filter((member) => member.status === "completed").length}/${progressAgents(item).length}`}</small></button>{!isAdminOnly(item) && (item.admin ? conversationAgents(item) : item.agents).map((member) => { const model = config?.profiles.find((candidate) => candidate.id === member.profile_id); return <button className={`lp-session-row ${activeTeam?.id === item.id && selection === member.id ? "selected" : ""}`} key={member.id} title={`${member.name} · ${model?.model || member.profile_id}`} onClick={() => selectTeam(item.id, member.id)}><ModelGlyph family={model?.family} /><span><strong>{member.session_id === item.admin?.session_id ? "ARIS · 主会话" : `${model?.name || member.profile_id} · ${member.name}`}</strong><small>{member.files[0] || `会话/${member.id}`}</small></span><i className={`aw-status-dot ${member.status}`} style={{ "--lane-color": ["#4c92d1", "#8976c4", "#38a38f", "#c29142"][index % 4] } as CSSProperties} /></button>; })}</section>)}{!taskList.length && <p className="aw-sidebar-empty">{state.loading ? "正在读取会话…" : search ? "没有匹配的会话" : "会话和子任务会保留在这里。"}</p>}</nav><footer className="lp-sidebar-footer"><button onClick={onChat}><ChatBubbleIcon />独立聊天</button><button onClick={() => startDraft(selectedTemplate)}><GearIcon />模板与智能体</button></footer></aside>
    <main className="aw-main">
      {(localError || state.error || harness.connectionError) && <div className="aw-global-error" role="alert"><span>{localError || state.error || harness.connectionError}</span><button className="aw-icon" aria-label="重试连接" onClick={() => { setLocalError(""); state.retry(); }}><ReloadIcon /></button></div>}
      {mode === "models" ? <><div className="aw-page-header"><div><span>工作区 / 设置</span><h1>模型与连接</h1></div><button className="aw-button" onClick={() => setMode(team ? "team" : "home")}>返回聊天</button>{savedDraft && <button className="aw-button primary" onClick={() => setCreateOpen(true)}>继续编辑团队</button>}</div><div className="aw-settings-body">{data && <KeiSettings token={data.token} onChanged={state.refreshConfig} />}</div></> : <>
        <ProcessRelay teams={state.teams} profiles={config?.profiles || []} selectedTeamId={activeTeam?.id} selectedAgentId={conversation?.id} onSelect={selectTeam} onOpenTunnel={openTunnel} />
        <header className="lp-conversation-head">
          <span className="lp-thread-tag">{activeTeam ? activeTeam.name : "新会话"}</span>
          <strong>{view === "tunnel" ? "角色协作" : view === "delivery" ? "最终交付" : conversation ? conversation.name : simple ? "ARIS" : activeTeam ? "全部会话" : "ARIS"}</strong>
          <span className="lp-head-context">{conversation ? profile?.model || conversation.profile_id : simple ? activeTeam?.admin?.model_id : activeTeam ? `${workers.length} 个子任务` : data?.workspace.split(/[\\/]/).filter(Boolean).at(-1)}</span>
          {activeTeam && <div className="lp-view-actions">
            <button aria-pressed={view === "chat"} onClick={() => setView("chat")}>对话</button>
            {relatedChannels.length > 0 && <button aria-pressed={view === "tunnel"} onClick={() => openTunnel(activeTeam.id, relatedChannels.at(-1)!.id)}>协作</button>}
            {finalAuthor && <button aria-pressed={view === "delivery"} onClick={() => setView("delivery")}>最终交付</button>}
            <button aria-pressed={view === "timeline"} onClick={() => setView("timeline")}>记录</button>
            {agent && <button aria-pressed={view === "browser"} onClick={() => setView("browser")}>浏览器</button>}
            <button aria-label="交付与回滚" title="交付与回滚" onClick={() => setDetails(true)}><FileTextIcon /></button>
          </div>}
        </header>
        {activeTeam ? <>
          {activity.error && <div className="aw-global-error" role="alert">执行记录读取失败：{activity.error}</div>}
          {view === "tunnel" && focusedTunnel
            ? <TeamTunnelThread key={focusedTunnel.id} team={activeTeam} tunnel={focusedTunnel} onSelectActor={selectAgent} onSelectTunnel={setTunnelId} />
            : view === "delivery" && data
            ? <Suspense fallback={<p className="lp-delivery-notice" role="status">正在读取最终交付…</p>}>
                <TeamDelivery key={activeTeam.id} team={activeTeam} token={data.token} onOpenTunnel={(id) => openTunnel(activeTeam.id, id)} />
              </Suspense>
            : view === "timeline"
            ? <div className="lp-content-scroll"><TeamTimeline team={activeTeam} sessions={activity.sessions} onSelectAgent={selectAgent} /></div>
            : view === "browser" && agent && data
            ? <div className="lp-content-scroll"><ComputerSettings token={data.token} agentId={agent.session_id || agent.id} /></div>
            : <ProcessChat team={activeTeam} sessions={activity.sessions} profiles={config?.profiles || []} agentId={conversation?.id} onSelectAgent={selectAgent} onOpenTunnel={(id) => openTunnel(activeTeam.id, id)} />}
          {activity.approvals.length > 0 && <section className="aw-approvals" aria-label="待确认的操作">
            {activity.approvals.map((approval) => <div key={approval.request_id}>
              <div><strong>{approval.name} 需要确认</strong><p>{approval.summary}</p></div>
              <button className="aw-button" onClick={() => void activity.resolve(approval.request_id, false)}>拒绝</button>
              <button className="aw-button primary" onClick={() => void activity.resolve(approval.request_id, true)}>允许这次操作</button>
            </div>)}
          </section>}
          {view === "chat" && !agent && finalAuthor?.status === "completed" && activeTeam.status === "completed" &&
            <button className="lp-open-final" onClick={() => setView("delivery")}><FileTextIcon />{finalAuthor.name} 已完成汇总 · 查看最终交付 →</button>}
          {data && ["completed", "failed", "cancelled"].includes(activeTeam.status) &&
            <WorkflowReview key={activeTeam.events.at(-1)?.at || activeTeam.admin?.request_id} token={data.token} subject="team_id" id={activeTeam.id}
              startedAt={activeTeam.events.filter((event) => event.kind === "started" || event.kind === "admin_started").at(-1)?.at} />}
          {view !== "tunnel" && view !== "delivery" && <div className="lp-composer-wrap">
            <MessageComposer key={activeTeam.id} agent={conversation} agents={simple ? undefined : workers}
              recipientLabel={activeTeam.admin ? "ARIS" : "整个团队"} files={activeTeam.files}
              running={!!running && !!agent && !adminBusy} busy={state.busy || !!adminBusy || (!agent && running)}
              onTargetChange={(id) => selectAgent(id || "overview")}
              onSend={(text) => agent ? state.message(activeTeam.id, text, agent.id) : state.admin(text, activeTeam.id, adminProfiles[activeTeam.id])} />
          </div>}
        </> : <><div className="lp-empty-chat"><Emblem size={30} /><p>开始一个新的会话</p><span>直接提问或交代任务，ARIS 会按需协作。</span>{!profiles.length && <button className="aw-button" onClick={() => setMode("models")}>连接 KEI API</button>}</div><div className="lp-composer-wrap"><MessageComposer key="new-chat" newChat initialText={quickGoal} onDraftChange={setQuickGoal} running={false} busy={state.busy} onSend={sendNewGoal} toolbar={<><select aria-label="选择工作流模板" value={templateId} onChange={(event) => setTemplateId(event.target.value)}><option value="">自动安排</option>{templates.map((template) => <option value={template.id} key={template.id}>{template.name}</option>)}</select><button type="button" className="lp-compose-action" onClick={() => startDraft(selectedTemplate)}>自定义</button></>} />{state.busy && <div className="lp-planning">正在接收消息…</div>}</div></>}
      </>}
    </main>
    {mode !== "models" && <WorkspaceTrajectory teams={state.teams} activeTeamId={activeTeam?.id} selectedAgentId={view === "tunnel" ? undefined : conversation?.id} profiles={config?.profiles || []} sessions={activity.sessions} onSelectAgent={selectTeam} onOpenTunnel={openTunnel} />}
    <footer className="lp-footer"><span>最近</span><div>{profiles.slice(0, 8).map((model) => <button key={model.id} title={model.model} onClick={() => { newChat(); setNewProfile(model.id); }}><ModelGlyph family={model.family} /></button>)}</div><small>{state.teams.length} 条任务线 · {state.teams.filter(teamIsActive).length} 个运行中</small></footer>
    <Dialog.Root open={details} onOpenChange={setDetails}><Dialog.Portal><Dialog.Overlay className="aw-dialog-overlay" /><Dialog.Content className="lp-details-dialog" aria-describedby={undefined}><Dialog.Title className="sr-only">交付与回滚</Dialog.Title>{activeTeam && <TeamDetails team={activeTeam} state={state} onClose={() => setDetails(false)} />}</Dialog.Content></Dialog.Portal></Dialog.Root>
    <Dialog.Root open={createOpen} onOpenChange={setCreateOpen}><Dialog.Portal><Dialog.Overlay className="aw-dialog-overlay" /><Dialog.Content className="aw-create-dialog" aria-describedby="aw-create-description"><header><div><Dialog.Title>模板与智能体</Dialog.Title><Dialog.Description id="aw-create-description">调整任务分工、依赖和每位智能体固定使用的模型。</Dialog.Description></div><Dialog.Close className="aw-icon" aria-label="关闭团队配置"><Cross1Icon /></Dialog.Close></header><div className="aw-create-scroll"><TeamComposer key={draftVersion} token={data?.token} config={config} busy={state.busy} initialGoal={quickGoal} initialTemplate={initialTemplate} initialDraft={savedDraft} onDraftChange={setSavedDraft} onConfigure={() => { setCreateOpen(false); setMode("models"); }} onCreate={async (draft) => { const created = await state.create(draft); if (created) { setCreateOpen(false); selectTeam(created.id); } }} /></div></Dialog.Content></Dialog.Portal></Dialog.Root>
  </div>;
}
