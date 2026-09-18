import { describe, expect, it } from "vitest";
import { DEVICETYPE_WEBGL2, DEVICETYPE_WEBGPU } from "playcanvas";
import { PREVIEW_DEVICE_TYPES, previewDeviceTypes } from "./PreviewBackend";

describe("preview graphics backend", () => {
  it("tries WebGPU before falling back to WebGL2", () => {
    expect(PREVIEW_DEVICE_TYPES).toEqual([DEVICETYPE_WEBGPU, DEVICETYPE_WEBGL2]);
    expect(previewDeviceTypes()).toEqual([DEVICETYPE_WEBGPU, DEVICETYPE_WEBGL2]);
  });

  it("returns a fresh device preference list for each application mount", () => {
    expect(previewDeviceTypes()).not.toBe(previewDeviceTypes());
  });
});
