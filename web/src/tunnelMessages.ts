import type { Team, TeamTunnel, TunnelMessage } from "./teamTypes";
import { actionLabels, messageAction } from "./collaborationActions";

export function tunnelMessageLabel(message: TunnelMessage) {
  return actionLabels[messageAction(message)];
}

export function mirrorsReceivedTunnelMessage(team: Team, recipient: string, noteId: string): boolean {
  return noteId.startsWith("tunnel:") && !!team.tunnels?.some((tunnel) =>
    tunnel.participants.includes(recipient) && tunnel.messages.some((message) =>
      message.to === recipient && `tunnel:${tunnel.id}:${message.id}` === noteId));
}

export function tunnelRequestState(tunnel: TeamTunnel, request: TunnelMessage): string | undefined {
  const invite = request.kind === "cowork_invite";
  const question = request.kind === "question" || (request.kind === "message" && request.expects_reply);
  if (!invite && !question && request.kind !== "proposal") return;
  const replies = tunnel.messages.filter((message) =>
    message.reply_to === request.id && message.from === request.to && message.to === request.from);
  if (invite) {
    if (replies.some((reply) => reply.kind === "cowork_result")) return "已交付";
    if (replies.some((reply) => reply.kind === "cowork_decline")) return "已婉拒";
    if (tunnel.status !== "open" || request.followup_closed_reason) return "未交付";
    return replies.some((reply) => reply.kind === "cowork_accept") ? "已接手" : "待接手";
  }
  if (request.kind === "proposal" && tunnel.decision?.proposal_id === request.id) return "已采纳";
  if (replies.some((reply) => reply.kind === "answer" || reply.kind === "message" || reply.kind === "accept")) return "已答复";
  return tunnel.status === "open" && !request.followup_closed_reason ? "待答复" : "未答复";
}
