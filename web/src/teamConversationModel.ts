import type { Team, TeamAgent } from "./teamTypes";

export const ADMIN_VIEW = "__aris_admin_conversation__";
export const teamIsActive = (team?: Team) => !!team && ["planning", "running", "cancelling"].includes(team.status);
export function adminAgent(team: Team): TeamAgent | undefined {
  if (!team.admin) return;
  const existing = team.agents.find((agent) => agent.session_id === team.admin!.session_id);
  const status = team.admin.phase === "planning" ? "running" : team.admin.phase === "failed" ? "failed" : team.admin.phase === "cancelled" ? "cancelled" : "completed";
  return { ...(existing || { id: ADMIN_VIEW, role: "admin", prompt: "", depends_on: [], files: [] }), name: "ARIS", profile_id: team.admin.profile_id, session_id: team.admin.session_id, status, output: team.admin.response, error: team.admin.error };
}
export function workerAgents(team: Team): TeamAgent[] {
  return team.admin ? team.agents.filter((agent) => agent.session_id !== team.admin!.session_id) : team.agents;
}
export function progressAgents(team: Team): TeamAgent[] {
  const workers = workerAgents(team);
  return team.execution_budget ? workers.filter((agent) => team.execution_budget!.agent_ids.includes(agent.id)) : workers;
}
/** A metadata-only admin is an actual persisted conversation, never an invented worker. */
export function conversationAgents(team: Team): TeamAgent[] {
  const admin = adminAgent(team);
  return admin ? [admin, ...workerAgents(team)] : team.agents;
}
export const isAdminOnly = (team?: Team) => !!team?.admin && workerAgents(team).length === 0;
export function messageSource(team: Team, message: NonNullable<Team["messages"]>[number]): "user" | "admin" | "unknown" {
  if (message.source) return message.source;
  // Older admin runs stored both dispatched assignments and human follow-ups
  // without authorship. Keep that uncertainty instead of impersonating a user.
  return team.admin && message.agent_id ? "unknown" : "user";
}
