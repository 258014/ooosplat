// @vitest-environment jsdom

import { beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({ invoke: vi.fn() }));

vi.mock("@tauri-apps/api/core", () => ({
  convertFileSrc: vi.fn(),
  invoke: mocks.invoke,
}));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn() }));
vi.mock("@tauri-apps/plugin-dialog", () => ({ confirm: vi.fn(), open: vi.fn(), save: vi.fn() }));
vi.mock("@tauri-apps/plugin-opener", () => ({ revealItemInDir: vi.fn() }));

import { checkColmapAcceleration, getAppRuntimeStatus, revealProject, revealProjectLogs } from "./backend";

describe("backend browser guards", () => {
  beforeEach(() => {
    delete (window as typeof window & { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__;
    mocks.invoke.mockReset();
  });

  it("does not invoke the acceleration command outside Tauri", async () => {
    await expect(checkColmapAcceleration()).resolves.toBeNull();
    expect(mocks.invoke).not.toHaveBeenCalled();
  });

  it("uses validated backend commands for runtime state and project folders", async () => {
    (window as typeof window & { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__ = {};
    mocks.invoke.mockResolvedValueOnce({ pipelineRunning: false, previewProjectId: null });
    await expect(getAppRuntimeStatus()).resolves.toEqual({ pipelineRunning: false, previewProjectId: null });
    await revealProject({ id: "project-id" } as Parameters<typeof revealProject>[0]);
    await revealProjectLogs("project-id");

    expect(mocks.invoke).toHaveBeenNthCalledWith(1, "get_app_runtime_status");
    expect(mocks.invoke).toHaveBeenNthCalledWith(2, "open_project_location", { projectId: "project-id", location: "project" });
    expect(mocks.invoke).toHaveBeenNthCalledWith(3, "open_project_location", { projectId: "project-id", location: "logs" });
  });
});
