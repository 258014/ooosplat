import { DEVICETYPE_WEBGL2, DEVICETYPE_WEBGPU } from "playcanvas";

export const PREVIEW_DEVICE_TYPES = [DEVICETYPE_WEBGPU, DEVICETYPE_WEBGL2] as const;

export function previewDeviceTypes() {
  return [...PREVIEW_DEVICE_TYPES];
}
