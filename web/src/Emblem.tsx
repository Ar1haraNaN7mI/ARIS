import type { VisualStyle } from "./types";

/** Original ARIS mark: separate angular paths form an A and a shared crossbar. */
export function Emblem({ size = 42, className = "" }: { size?: number; className?: string; visualStyle?: VisualStyle }) {
  return <svg className={`aris-emblem ${className}`} width={size} height={size} viewBox="0 0 100 100" role="img" aria-label="ARIS 标志" style={{ flexShrink: 0 }}>
    <path fill="currentColor" d="M48 10 8 88h17l31-61Z" />
    <path fill="currentColor" d="m61 36-9 18 18 34h18Z" />
    <path fill="currentColor" d="m39 63-7 14h34l-7-14Z" />
    <path fill="currentColor" opacity=".5" d="M28 88h35l4 8H24Z" />
  </svg>;
}
