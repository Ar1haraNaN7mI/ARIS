import type { CollaborationAction } from "./collaborationActions";

// Each outline fits the 18px lane marker. Invitations, acceptances and
// declines share a person outline; their action marks stay distinct in monochrome.
const person = "M10 9a3 3 0 1 0-6 0 3 3 0 0 0 6 0ZM2 18v-1a5 5 0 0 1 10 0v1";
const outlines: Record<CollaborationAction, string> = {
  message: "M3 3.5h14v10H9l-5 3.5v-3.5H3z M6 7h8 M6 10h5",
  notice: "M4 8h4l7-4v12l-7-4H4z M6 12l1.5 5h3l-2-5 M17 8v4",
  question: "M10 2.5a7 7 0 0 1 0 14H8l-4.5 2 1.2-4.4A7 7 0 0 1 10 2.5Z M8.2 7.3a2 2 0 1 1 3.2 1.6c-1.1.7-1.4 1.1-1.4 2 M10 13h.01",
  answer: "M8 4 2.5 9.5 8 15 M3 9.5h7a6 6 0 0 1 6 6v.5",
  proposal: "M7 13c0-2-2.5-2.8-2.5-6a5.5 5.5 0 0 1 11 0c0 3.2-2.5 4-2.5 6z M7.8 15.6h4.4 M9 18h2",
  accept: "m10 2 6.5 2.5v5.8c0 3.6-3.4 6.3-6.5 7.8-3.1-1.5-6.5-4.2-6.5-7.8V4.5z M6.8 9.8l2.1 2.1 4.5-4.6",
  cowork_invite: person + " M15 4v6 M12 7h6",
  cowork_accept: person + " M12 7l2.2 2.2 4-4.5",
  cowork_decline: person + " M13 5l4.5 4.5 M17.5 5 13 9.5",
  cowork_result: "M3 12v4.5h14V12 M10 2.5v9.2 M6.5 8.2l3.5 3.5 3.5-3.5",
  cowork_turn_started: "M2.5 7h15v10h-15z M7 7V4h6v3 M8.5 9.4l4 2.6-4 2.6z",
  cowork_turn_finished: "M3 16V4h5l2 2h6v2 M17 10v1a3.5 3.5 0 0 1-3.5 3.5H8 M10.5 11.5l-3 3 3 3.1",
  open: "M8 4H3v12h5 M10 10h8 M15 7l3 3-3 3",
  close: "M3 10h8 M6 7l-3 3 3 3 M12 4h5v12h-5",
  read: "M1.5 10S4.8 4.7 10 4.7s8.5 5.3 8.5 5.3-3.3 5.3-8.5 5.3S1.5 10 1.5 10Z M12.1 10a2.1 2.1 0 1 0-4.2 0 2.1 2.1 0 0 0 4.2 0Z",
  resume: "M11.5 1.8 4.5 11H9l-1 7.2 7.5-10H11z",
  end_followup: "M3 3v7a4 4 0 0 0 4 4h7 M14 10v8",
  handoff: "M2 5h9v5H2z M8 13h9 M14 10l3 3-3 3",
  dispatch: "M8.5 2h3v3h-3z M10 5v4 M4.5 13V9h11v4 M2.5 13h4v4h-4z M13.5 13h4v4h-4z",
  create: "M3 3.5h10v13H3z M6 7h4 M6 10h2 M15.5 9.5v7 M12 13h7",
};

export function CollaborationIcon({ action, size = 16 }: { action: CollaborationAction; size?: number }) {
  return <svg className="collaboration-icon" data-action-icon={action} aria-hidden="true" focusable="false"
    width={size} height={size} viewBox="0 0 20 20" fill="none" stroke="currentColor"
    strokeWidth="1.65" strokeLinecap="round" strokeLinejoin="round"><path d={outlines[action]} /></svg>;
}
