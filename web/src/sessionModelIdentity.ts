import type { SessionEvent } from "./types";
import type { ModelFamily, ModelProfile } from "./teamTypes";

export type SessionModelIdentity = { profileId?: string; modelId?: string; family?: ModelFamily };
const families = new Set(["deepseek", "grok", "gemini", "gpt", "glm", "kimi"]);

/** Identity belongs to a frozen turn. Missing legacy metadata stays unknown;
 * changing an agent's current profile must never relabel earlier messages. */
export function sessionModelIdentities(events: SessionEvent[], profiles: ModelProfile[] = []) {
  const identities = new Map<string, SessionModelIdentity>();
  let current: SessionModelIdentity | undefined;
  for (const event of events) {
    if (event.type === "turn_start") {
      const exactProfile = profiles.find((profile) => profile.id === event.profile_id && profile.model === event.model_id);
      current = event.profile_id || event.model_id || event.family ? {
        profileId: event.profile_id || undefined,
        modelId: event.model_id || undefined,
        family: event.family && families.has(event.family) ? event.family as ModelFamily : exactProfile?.family,
      } : undefined;
    }
    if (current) identities.set(event.id, current);
    if (event.type === "turn_end") current = undefined;
  }
  return identities;
}
