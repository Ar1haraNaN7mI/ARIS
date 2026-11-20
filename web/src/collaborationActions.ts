import type { TunnelMessage } from "./teamTypes";

export type CollaborationAction = TunnelMessage["kind"] | "open" | "close" | "read" | "resume" |
  "end_followup" | "handoff" | "dispatch" | "create" | "cowork_turn_started" | "cowork_turn_finished";

export const actionLabels: Record<CollaborationAction, string> = {
  message: "发言", notice: "通知", question: "请求答复", answer: "答复",
  proposal: "提议", accept: "同意提议", cowork_invite: "协作邀请",
  cowork_accept: "接受协作", cowork_decline: "婉拒协作", cowork_result: "交付协作成果",
  open: "开启通道", close: "关闭通道", read: "读取通道", resume: "唤醒泳道",
  end_followup: "结束跟进", handoff: "交接", dispatch: "分工", create: "创建角色",
  cowork_turn_started: "独立协作开工", cowork_turn_finished: "返回主任务",
};

export function messageAction(message: TunnelMessage): TunnelMessage["kind"] {
  if (message.kind === "message") {
    if (message.expects_reply) return "question";
    if (message.reply_to) return "answer";
    if (message.expects_reply === false) return "notice";
  }
  return Object.hasOwn(actionLabels, message.kind) ? message.kind : "message";
}

export function toolAction(name: string | undefined, args: unknown): CollaborationAction | undefined {
  if (name === "create_sub_agents") return "create";
  if (name === "dispatch_sub_agents") return "dispatch";
  if (name !== "agent_tunnel" || !args || typeof args !== "object" || !("action" in args)) return;
  switch (args.action) {
    case "open": case "close": case "read": case "accept": return args.action;
    case "send":
      if ("expects_reply" in args && args.expects_reply === true) return "question";
      // A reference may resolve to an answer or a passive acknowledgment on
      // the server. The persisted peer message carries the definitive kind.
      return "reply_to" in args && args.reply_to ? "message" : "notice";
    case "ask": return "question";
    case "propose": return "proposal";
    case "invite": return "cowork_invite";
    case "accept_work": return "cowork_accept";
    case "decline_work": return "cowork_decline";
    case "complete_work": return "cowork_result";
  }
}
