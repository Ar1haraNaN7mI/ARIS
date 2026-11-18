import { createElement } from "react";
import type { ModelProfile } from "./teamTypes";
import { MODEL_BRAND_MARKS } from "./vendor/lancer-light-ui/model-brand-marks";

export function AgentGlyph({ family, profileId, profiles = [], size = 14, className = "" }: {
  family?: string; profileId?: string; profiles?: ModelProfile[]; size?: number; className?: string;
}) {
  const resolved = family || profiles.find((profile) => profile.id === profileId)?.family || profileId || "gpt";
  const key = resolved === "gpt" ? "openai" : resolved === "glm" ? "zhipu" : resolved;
  const icon = MODEL_BRAND_MARKS[key];
  return <span className={`agent-glyph ${className}`} aria-hidden="true" style={{ width: size, height: size, display: "inline-flex", flex: "none", alignItems: "center", justifyContent: "center" }}>
    {icon ? <svg {...icon.attributes} width={size} height={size}>{icon.parts.map((part, index) => createElement(part.tag, { ...part.attributes, key: index }))}</svg> : <svg viewBox="0 0 16 16" width={size} height={size} fill="none" stroke="currentColor"><rect x="3" y="3" width="10" height="10" rx="3" /><path d="M5 6h6M5 9h4" /></svg>}
  </span>;
}
