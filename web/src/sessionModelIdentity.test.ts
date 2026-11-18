import { describe, expect, it } from "vitest";
import { sessionModelIdentities } from "./sessionModelIdentity";
import type { SessionEvent } from "./types";
import type { ModelProfile } from "./teamTypes";
const event = (id: string, type: string, extra: Partial<SessionEvent> = {}): SessionEvent => ({ id, type, at: "2026-11-19T00:00:00Z", ...extra });
describe("Frozen turn model identity", () => {
  it("keeps historical identity after an agent switches profile", () => {
    const events = [event("first", "turn_start", { profile_id: "switchable", model_id: "gpt-old", family: "gpt" }), event("answer1", "assistant_message"), event("end", "turn_end"), event("second", "turn_start", { profile_id: "switchable", model_id: "gemini-new", family: "gemini" }), event("tool", "tool_call"), event("answer2", "assistant_message")];
    const profiles: ModelProfile[] = [{ id: "switchable", name: "Changed profile", family: "gemini", model: "gemini-new", protocol: "chat_completions" }];
    const identities = sessionModelIdentities(events, profiles);
    expect(identities.get("answer1")).toMatchObject({ family: "gpt", modelId: "gpt-old" });
    expect(identities.get("tool")).toMatchObject({ family: "gemini", modelId: "gemini-new" });
    expect(identities.get("answer2")).toMatchObject({ family: "gemini", modelId: "gemini-new" });
  });
  it("leaves old and metadata-free turns unknown", () => {
    const events = [event("legacy", "assistant_message"), event("known", "turn_start", { model_id: "gpt-real", family: "gpt" }), event("known-answer", "assistant_message"), event("missing", "turn_start"), event("unknown-answer", "assistant_message")];
    const identities = sessionModelIdentities(events);
    expect(identities.has("legacy")).toBe(false);
    expect(identities.get("known-answer")?.modelId).toBe("gpt-real");
    expect(identities.has("unknown-answer")).toBe(false);
  });
});
