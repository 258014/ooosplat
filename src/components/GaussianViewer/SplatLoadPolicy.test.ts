import { describe, expect, it } from "vitest";
import {
  EDITABLE_SPLAT_ASSET_OPTIONS,
  requiredSplatTextureSide,
  splatTextureCapacityError,
} from "./SplatLoadPolicy";

describe("editable Gaussian splat load policy", () => {
  it("keeps source vertex order so edit masks map to the original PLY", () => {
    expect(EDITABLE_SPLAT_ASSET_OPTIONS).toEqual({ data: { reorder: false } });
  });

  it("calculates the square texture capacity used by PlayCanvas", () => {
    expect(requiredSplatTextureSide(0)).toBe(0);
    expect(requiredSplatTextureSide(4_000_000)).toBe(2000);
    expect(requiredSplatTextureSide(4_000_001)).toBe(2001);
  });

  it("rejects a model before loading when it exceeds the device texture limit", () => {
    expect(splatTextureCapacityError(16_777_216, 4096)).toBeNull();
    expect(splatTextureCapacityError(16_777_217, 4096)).toContain("需要至少 4097");
  });
});
