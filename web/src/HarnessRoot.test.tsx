import { act, cleanup, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { HarnessRoot } from "./HarnessRoot";
import * as api from "./api";
import type { AccessStatus, Bootstrap } from "./types";

vi.mock("./App", () => ({ App: () => <div>ARIS private workbench</div> }));
vi.mock("./api", async (original) => ({ ...(await original<typeof import("./api")>()), accessStatus: vi.fn(), bootstrap: vi.fn() }));
const locked: AccessStatus = { enabled: true, unlocked: false, token: "token", profile: { username: "Operator", badge_id: "ARIS-01" } };
beforeEach(() => {
  vi.stubGlobal("matchMedia", vi.fn(() => ({ matches: false, addEventListener: vi.fn(), removeEventListener: vi.fn() })));
  vi.mocked(api.bootstrap).mockResolvedValue({ token: "token", profile: locked.profile } as Bootstrap);
});
afterEach(() => { cleanup(); vi.resetAllMocks(); vi.unstubAllGlobals(); });
describe("ARIS direct entry", () => {
  it("opens the workbench without loading a startup iframe", async () => {
    vi.mocked(api.accessStatus).mockResolvedValue({ ...locked, unlocked: true });
    render(<HarnessRoot />);
    expect(await screen.findByText("ARIS private workbench")).toBeTruthy();
    expect(document.querySelector("iframe")).toBeNull();
    expect(api.bootstrap).toHaveBeenCalledOnce();
  });
  it("does not request private data while locked and removes it when revoked", async () => {
    vi.mocked(api.accessStatus).mockResolvedValue({ ...locked, unlocked: true });
    render(<HarnessRoot />);
    await screen.findByText("ARIS private workbench");
    vi.mocked(api.accessStatus).mockResolvedValue(locked);
    await act(async () => window.dispatchEvent(new Event("aris-access-locked")));
    expect(await screen.findByLabelText("输入密码")).toBeTruthy();
    expect(screen.queryByText("ARIS private workbench")).toBeNull();
    expect(api.bootstrap).toHaveBeenCalledOnce();
  });
  it("shows an access error rather than opening the workbench", async () => {
    vi.mocked(api.accessStatus).mockRejectedValue(new Error("service offline"));
    render(<HarnessRoot />);
    expect((await screen.findByRole("alert")).textContent).toContain("service offline");
    expect(api.bootstrap).not.toHaveBeenCalled();
  });
});
