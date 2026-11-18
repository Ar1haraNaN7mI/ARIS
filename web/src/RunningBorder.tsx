import "./vendor/lancer-light-ui/src/running-border.css";

/** Lancer light-ui's execution outline, rendered only for actual running work. */
export function RunningBorder({ radius = 5 }: { radius?: number }) {
  return <svg className="light-running-border" aria-hidden="true" focusable="false"><rect x="1" y="1" width="calc(100% - 2px)" height="calc(100% - 2px)" rx={radius} pathLength="100" /></svg>;
}
