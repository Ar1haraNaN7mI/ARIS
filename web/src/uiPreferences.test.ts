import { afterEach, describe, expect, it, vi } from "vitest";
import { defaultUiPreferences, readUiPreferences, resetUiPreferences, sanitizeUiPreferences, shouldSend, updateUiPreferences } from "./uiPreferences";
import { noticeForEvent } from "./notifications";
import type { Envelope } from "./types";

afterEach(() => { vi.restoreAllMocks(); resetUiPreferences(); });
describe("persisted interface preferences", () => {
  it("rejects malformed imports while keeping valid settings", () => {
    expect(sanitizeUiPreferences({theme: "invalid", accent: "url(https://example.com)", sendKey: "mod-enter", notificationSound: "true"}))
      .toEqual({...defaultUiPreferences, sendKey: "mod-enter"});
  });
  it("does not publish a preference when storage fails", () => {
    resetUiPreferences();
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => { throw new Error("blocked"); });
    expect(() => updateUiPreferences({theme:"dark"})).toThrow("blocked");
    expect(readUiPreferences().theme).toBe(defaultUiPreferences.theme);
  });
});
describe("real input and notification behavior", () => {
  const enter = {key:"Enter", shiftKey:false, ctrlKey:false, metaKey:false, altKey:false, isComposing:false};
  it("keeps IME and newline entry safe for both sending modes", () => {
    expect(shouldSend(enter, "enter")).toBe(true);
    expect(shouldSend({...enter,isComposing:true}, "enter")).toBe(false);
    expect(shouldSend({...enter,shiftKey:true}, "enter")).toBe(false);
    expect(shouldSend(enter, "mod-enter")).toBe(false);
    expect(shouldSend({...enter,ctrlKey:true}, "mod-enter")).toBe(true);
    expect(shouldSend({...enter,metaKey:true}, "mod-enter")).toBe(true);
  });
  it("creates only requested lifecycle notifications without leaking task content", () => {
    const event: Envelope = {sequence:12,event_type:"agent.done",occurred_at:"",payload:{session_id:"s1",text:"private contents"}};
    expect(noticeForEvent(event, "completed")).toEqual({id:12,title:"ARIS 任务已完成",sessionId:"s1"});
    for (const state of [undefined, "failed", "paused", "cancelled", "running"]) expect(noticeForEvent(event, state)).toBeUndefined();
    updateUiPreferences({notifyOnCompletion:false});
    expect(noticeForEvent(event, "completed")).toBeUndefined();
    expect(noticeForEvent({...event,event_type:"agent.approval_needed"})?.title).toContain("授权");
    updateUiPreferences({notifyOnApproval:false});
    expect(noticeForEvent({...event,event_type:"agent.approval_needed"})).toBeUndefined();
  });
});
