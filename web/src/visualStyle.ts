import { useLayoutEffect, useSyncExternalStore } from "react";
import { post } from "./api";
import type { Profile, VisualStyle } from "./types";

let currentStyle: VisualStyle = "rhine";
const listeners = new Set<() => void>();
export const normalizeVisualStyle = (value: unknown): VisualStyle => value === "laterano" ? "laterano" : "rhine";
export const readVisualStyle = () => currentStyle;

/** The shared Rust profile is authoritative; no browser-only preference copy. */
export function acceptVisualProfile(profile: Pick<Profile, "visual_style">) {
  const next = normalizeVisualStyle(profile.visual_style);
  if (currentStyle === next) return;
  currentStyle = next;
  for (const listener of listeners) listener();
}
function subscribe(listener: () => void) {
  listeners.add(listener);
  return () => { listeners.delete(listener); };
}
export function useVisualStyle() {
  return useSyncExternalStore(subscribe, readVisualStyle, () => "rhine" as const);
}

export async function saveVisualStyle(token: string, visual_style: VisualStyle) {
  // Patch only this setting: editing appearance must never overwrite identity.
  const profile = await post<Profile>("/api/profile", token, { visual_style });
  if (profile?.visual_style !== visual_style)
    throw new Error("本机服务未保存此 UI 风格，请更新 ARIS 后重试。");
  return profile;
}

export function useVisualStyleEffects() {
  const style = useVisualStyle();
  useLayoutEffect(() => {
    // Legacy profile values remain readable; ARIS has one coherent identity.
    document.documentElement.dataset.visualStyle = "aris";
    {
      let favicon = document.querySelector<HTMLLinkElement>('link[data-aris-favicon]');
      if (!favicon) {
        favicon = document.createElement("link");
        favicon.rel = "icon";
        favicon.type = "image/svg+xml";
        favicon.dataset.arisFavicon = "";
        document.head.append(favicon);
      }
      favicon.href = "/aris.svg";
    }
    const meta = document.querySelector<HTMLMetaElement>('meta[name="theme-color"]');
    if (meta) meta.content = "#eef8fb";
    const desktop = window as typeof window & {
      __ARIS_DESKTOP__?: boolean;
      ipc?: { postMessage?: (message: string) => void };
    };
    if (desktop.__ARIS_DESKTOP__ && typeof desktop.ipc?.postMessage === "function")
      desktop.ipc.postMessage(JSON.stringify({ event: "aris-visual-style", visual_style: style }));
  }, [style]);
}
