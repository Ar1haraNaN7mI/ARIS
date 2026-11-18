import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { KeiSettings } from "./KeiSettings";
import * as api from "./api";

vi.mock("./api", async (original) => ({ ...(await original<typeof import("./api")>()), rpc: vi.fn() }));
const config = { base_url: "http://127.0.0.1:31880/v1", configured: false, profiles: [] };
const authorizeUrl = "http://127.0.0.1:31880/aris/import?state=test-state";

afterEach(() => { cleanup(); vi.restoreAllMocks(); vi.resetAllMocks(); vi.unstubAllGlobals(); });

describe("KEI import browser integration", () => {
  it("refreshes the surrounding model choices when revisiting after an external import", async () => {
    const onChanged = vi.fn();
    vi.mocked(api.rpc).mockResolvedValue(config);
    render(<KeiSettings token="test-token" onChanged={onChanged} />);
    await waitFor(() => expect(onChanged).toHaveBeenCalledOnce());
    expect(screen.getByLabelText("API Base URL")).toBeTruthy();
  });

  it("opens the native external authorization URL without a blank popup and signals completion", async () => {
    vi.stubGlobal("__ARIS_DESKTOP__", true);
    const postMessage = vi.fn(); vi.stubGlobal("ipc", { postMessage });
    const open = vi.spyOn(window, "open").mockReturnValue(null);
    vi.mocked(api.rpc).mockImplementation(async (_token, method) => {
      if (method === "kei/import/start") return { url: authorizeUrl };
      if (method === "kei/import/status") return { status: "completed", imported_count: 2 };
      return config;
    });
    render(<KeiSettings token="test-token" />);
    fireEvent.click(await screen.findByRole("button", { name: "从 KEIAPI 一键导入" }));
    await waitFor(() => expect(postMessage).toHaveBeenCalledWith(JSON.stringify({ event: "aris-imported", version: 1 })));
    expect(open).toHaveBeenCalledExactlyOnceWith(authorizeUrl, "_blank");
    expect((await screen.findByRole("status")).textContent).toContain("2");
  });

  it("reserves a browser popup synchronously then navigates after receiving the URL", async () => {
    const popup = { opener: "parent", location: { href: "about:blank" }, close: vi.fn() };
    const open = vi.spyOn(window, "open").mockReturnValue(popup as unknown as Window);
    let resolveStart: (value: unknown) => void = () => {};
    vi.mocked(api.rpc).mockImplementation(async (_token, method) => {
      if (method === "kei/import/start") return new Promise((resolve) => { resolveStart = resolve; });
      if (method === "kei/import/status") return { status: "pending" };
      return config;
    });
    render(<KeiSettings token="test-token" />);
    fireEvent.click(await screen.findByRole("button", { name: "从 KEIAPI 一键导入" }));
    expect(open).toHaveBeenCalledExactlyOnceWith("about:blank", "_blank");
    expect(popup.opener).toBeNull();
    resolveStart({ url: authorizeUrl });
    await waitFor(() => expect(popup.location.href).toBe(authorizeUrl));
    expect(screen.getByRole("link", { name: "未打开时点击这里" })).toBeTruthy();
  });
});
