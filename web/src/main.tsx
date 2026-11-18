import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { HarnessRoot } from "./HarnessRoot";
import "./styles.css";
import "./ui-effects.css";
import "./aris-theme.css";

createRoot(document.getElementById("root")!).render(
  <StrictMode><HarnessRoot /></StrictMode>,
);
