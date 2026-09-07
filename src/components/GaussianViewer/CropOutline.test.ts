import { describe, expect, it } from "vitest";
import { buildCropOutline } from "./CropOutline";

describe("buildCropOutline", () => {
  it("builds only the twelve edges of an axis-aligned box", () => {
    const geometry = buildCropOutline({ kind: "box", center: [1, 2, 3], size: [4, 6, 8] });

    expect(geometry.positions).toHaveLength(24);
    expect(geometry.colors).toHaveLength(24);
    expect(geometry.positions[0].toArray()).toEqual([-1, -1, -1]);
    expect(geometry.positions[1].toArray()).toEqual([3, -1, -1]);
  });

  it("builds three great circles for a sphere without gizmo geometry", () => {
    const geometry = buildCropOutline({ kind: "sphere", center: [2, 3, 4], radius: 5 });

    expect(geometry.positions).toHaveLength(3 * 72 * 2);
    expect(geometry.colors).toHaveLength(geometry.positions.length);
    expect(geometry.positions[0].toArray()).toEqual([7, 3, 4]);
  });
});
