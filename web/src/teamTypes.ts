export type ModelFamily = "deepseek" | "grok" | "gemini" | "gpt" | "glm" | "kimi";
export type ModelProfile = {
  id: string; name: string; family: ModelFamily; model: string;
  protocol: "chat_completions" | "responses";
  base_url?: string; api_key_env?: string; thinking?: boolean;
  max_tokens?: number; temperature?: number; extra_body?: Record<string, unknown>;
};
export type KeiConfig = { base_url: string; profiles: ModelProfile[]; configured: boolean };
export type TeamAgent = {
  id: string; name: string; role: string; profile_id: string; prompt: string;
  depends_on: string[]; files: string[]; session_id?: string | null;
  status: "pending" | "running" | "completed" | "failed" | "cancelled";
  output?: string | null; error?: string | null;
};
export type Team = {
  id: string; name: string; goal: string;
  status: "idle" | "running" | "cancelling" | "completed" | "failed" | "cancelled";
  agents: TeamAgent[]; files: string[];
  handoffs: { id: string; from: string; to: string; instruction: string; summary: string; at: string }[];
  checkpoints: { id: string; label: string; at: string; files: { path: string; exists: boolean; size: number }[] }[];
  events: { at: string; agent_id?: string | null; kind: string; message: string }[];
  messages?: { id: string; agent_id?: string | null; text: string; at: string; queued?: boolean; cascade?: boolean }[];
  created_at: string;
};
export type AgentDraft = Pick<TeamAgent, "id" | "name" | "role" | "profile_id" | "prompt" | "depends_on" | "files">;
export type TeamDraft = { name: string; goal: string; agents: AgentDraft[]; files: string[] };
export type TeamTemplate = TeamDraft & { id: string; description: string; category: string; builtin?: boolean };
export const statusLabels: Record<string, string> = {
  idle: "待开始", pending: "待执行", running: "执行中", cancelling: "正在停止", completed: "已完成", failed: "未完成", cancelled: "已停止",
};
export const familyLabels: Record<ModelFamily, string> = {
  deepseek: "DeepSeek", grok: "Grok", gemini: "Gemini", gpt: "GPT / Codex", glm: "GLM", kimi: "Kimi",
};
