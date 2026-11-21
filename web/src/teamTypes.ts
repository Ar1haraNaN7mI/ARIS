export type ModelFamily = "deepseek" | "grok" | "gemini" | "gpt" | "glm" | "kimi";
export type ModelProfile = {
  id: string; name: string; family: ModelFamily; model: string;
  protocol: "chat_completions" | "responses";
  base_url?: string; api_key_env?: string; thinking?: boolean;
  max_tokens?: number; temperature?: number; extra_body?: Record<string, unknown>;
};
export type KeiConfig = { base_url: string; profiles: ModelProfile[]; configured: boolean };
export type TunnelMessage = {
  id: string; seq: number; from: string; to: string; text: string; at: string;
  data?: unknown;
  data_json?: string;
  kind: "message" | "notice" | "question" | "answer" | "proposal" | "accept" | "cowork_invite" | "cowork_accept" | "cowork_decline" | "cowork_result";
  expects_reply?: boolean; reply_to?: string | null; proposal_id?: string | null; followup_closed_reason?: string | null;
};
export type TeamTunnel = {
  id: string; participants: string[]; topic: string; status: "open" | "closed" | "cancelled" | "interrupted";
  messages: TunnelMessage[]; decision?: { proposal_id: string; text: string; accepted_by: string[]; at: string } | null;
  created_at: string; updated_at: string; close_reason?: string | null;
};
export type TeamAgent = {
  id: string; name: string; role: string; profile_id: string; prompt: string;
  delivery?: "final" | "supporting" | "independent" | null;
  depends_on: string[]; files: string[]; session_id?: string | null;
  subagent_id?: string | null; task_id?: string | null; run_id?: string | null;
  status: "pending" | "running" | "completed" | "failed" | "cancelled";
  output?: string | null; error?: string | null;
};
export type Team = {
  id: string; name: string; goal: string;
  status: "idle" | "planning" | "running" | "cancelling" | "completed" | "failed" | "cancelled";
  admin?: { session_id: string; profile_id: string; model_id: string; family: ModelFamily; phase: "planning" | "answered" | "delegated" | "failed" | "cancelled"; response: string; error?: string; request_id?: string; requested_effort?: string; applied_effort?: string };
  execution_budget?: { agent_ids: string[]; max_agent_runs: number; max_steps_per_agent: number; max_tokens_per_step: number; max_parallel: number; max_runtime_secs: number; finalization_window_secs?: number };
  agents: TeamAgent[]; files: string[]; tunnels?: TeamTunnel[];
  handoffs: { id: string; from: string; to: string; instruction: string; summary: string; at: string }[];
  checkpoints: { id: string; label: string; at: string; files: { path: string; exists: boolean; size: number }[] }[];
  events: { at: string; agent_id?: string | null; kind: string; message: string }[];
  messages?: { id: string; agent_id?: string | null; text: string; at: string; queued?: boolean; cascade?: boolean; source?: "user" | "admin" | null }[];
  created_at: string;
};
export type AgentDraft = Pick<TeamAgent, "id" | "name" | "role" | "profile_id" | "prompt" | "depends_on" | "files" | "delivery">;
export type TeamDraft = { name: string; goal: string; agents: AgentDraft[]; files: string[] };
export type TeamTemplate = TeamDraft & { id: string; description: string; category: string; builtin?: boolean };
export const statusLabels: Record<string, string> = {
  idle: "待开始", planning: "正在处理", pending: "待执行", running: "执行中", cancelling: "正在停止", completed: "已完成", failed: "未完成", cancelled: "已停止",
};
export const familyLabels: Record<ModelFamily, string> = {
  deepseek: "DeepSeek", grok: "Grok", gemini: "Gemini", gpt: "GPT / Codex", glm: "GLM", kimi: "Kimi",
};
