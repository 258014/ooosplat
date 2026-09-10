import { describe, expect, it } from "vitest";
import {
  clampGuideZoom,
  findReshootRegion,
  GUIDE_ZOOM_MAX,
  GUIDE_ZOOM_MIN,
  guideCaption,
  guideMarkerRadius,
  guideZoomLabel,
  isSameReshootRegion,
  regionGeometryLabel,
  regionKey,
  reshootGuideArrows,
  reshootGuidance,
  reshootRegionsGuidance,
  shootingDirections,
  zoomFromWheel,
  zoomInGuide,
  zoomOutGuide,
  type ReshootRegion,
} from "./ReshootGuidance";

const sphere = (center: [number, number, number], radius = 0.5): ReshootRegion => ({ kind: "sphere", center, radius });
const box = (center: [number, number, number], size: [number, number, number] = [1, 1, 1]): ReshootRegion =>
  ({ kind: "box", center, size });

describe("reshoot region identity", () => {
  it("treats an identical selection as the same region", () => {
    expect(isSameReshootRegion(sphere([1, 2, 3]), sphere([1, 2, 3]))).toBe(true);
    expect(regionKey(sphere([1, 2, 3]))).toBe(regionKey(sphere([1, 2, 3])));
  });

  it("ignores sub-millimetre jitter so a repeated click is still a duplicate", () => {
    expect(isSameReshootRegion(sphere([1, 2, 3], 0.5), sphere([1.0004, 2, 3], 0.5002))).toBe(true);
  });

  it("separates regions that differ in position, size, or kind", () => {
    expect(isSameReshootRegion(sphere([1, 2, 3]), sphere([1, 2, 3.5]))).toBe(false);
    expect(isSameReshootRegion(sphere([1, 2, 3], 0.5), sphere([1, 2, 3], 0.9))).toBe(false);
    expect(isSameReshootRegion(sphere([1, 2, 3]), box([1, 2, 3]))).toBe(false);
    expect(isSameReshootRegion(box([0, 0, 0], [1, 2, 3]), box([0, 0, 0], [1, 2, 4]))).toBe(false);
  });

  it("finds an existing duplicate before it is added again", () => {
    const regions = [sphere([0, 0, 0]), box([4, 0, 1])];
    expect(findReshootRegion(regions, box([4, 0, 1]))).toBe(1);
    expect(findReshootRegion(regions, sphere([0, 0, 0]))).toBe(0);
    expect(findReshootRegion(regions, sphere([9, 9, 9]))).toBe(-1);
  });
});

describe("reshoot guidance text", () => {
  it("describes the selection geometry so the shooter knows which spot it means", () => {
    expect(regionGeometryLabel(sphere([1.5, -2, 0.25], 0.75))).toBe("球选 · 中心 (1.50, -2.00, 0.25) · 半径 0.75");
    expect(regionGeometryLabel(box([0, 0, 0], [2, 3, 4]))).toBe("盒选 · 中心 (0.00, 0.00, 0.00) · 尺寸 (2.00, 3.00, 4.00)");
  });

  it("never repeats the same wording for two different regions", () => {
    const regions = [sphere([0, 0, 0]), sphere([0, 0, 0.4]), sphere([1, 0, 0])];
    const guidance = reshootRegionsGuidance(regions);

    expect(new Set(guidance).size).toBe(regions.length);
    expect(guidance[0]).toContain("区域 1");
    expect(guidance[1]).toContain("区域 2");
    expect(guidance[2]).toContain("区域 3");
    expect(guidance[0]).toContain("中心 (0.00, 0.00, 0.00)");
    expect(guidance[1]).toContain("中心 (0.00, 0.00, 0.40)");
  });

  it("differs between a sphere and a box selection at the same spot", () => {
    expect(reshootGuidance(sphere([0, 0, 0]), 0)).not.toBe(reshootGuidance(box([0, 0, 0]), 0));
    expect(reshootGuidance(box([0, 0, 0]), 0)).toContain("正面、左侧、右侧与上方");
  });
});

describe("reshoot shooting directions", () => {
  it("gives a sphere region two rings of positions", () => {
    const directions = shootingDirections(sphere([0, 0, 0]));
    expect(directions.length).toBe(12);
    expect(directions.filter((item) => item.label.includes("抬高 30°"))).toHaveLength(4);
  });

  it("gives a box region the documented sides", () => {
    expect(shootingDirections(box([0, 0, 0])).map((item) => item.label)).toEqual(["正面", "左侧", "右侧", "右前 30°", "左前 30°"]);
  });
});

describe("reshoot guide layout", () => {
  it("points every arrow at the circled region from its shooting position", () => {
    const marker = { x: 400, y: 300, radius: 80 };
    const arrows = reshootGuideArrows(marker, shootingDirections(sphere([0, 0, 0])));

    expect(arrows).toHaveLength(12);
    for (const arrow of arrows) {
      const tipDistance = Math.hypot(arrow.toX - marker.x, arrow.toY - marker.y);
      const tailDistance = Math.hypot(arrow.fromX - marker.x, arrow.fromY - marker.y);
      expect(tipDistance).toBeCloseTo(Math.max(marker.radius + 18, 42), 6);
      expect(tailDistance).toBeGreaterThan(tipDistance);
      expect(arrow.label).not.toBe("");
    }
  });

  it("keeps arrows outside the highlight even for a tiny marker", () => {
    const arrows = reshootGuideArrows({ x: 0, y: 0, radius: 1 }, shootingDirections(box([0, 0, 0])));
    expect(arrows.every((arrow) => Math.hypot(arrow.toX, arrow.toY) >= 42)).toBe(true);
  });

  it("scales the highlight with the selection and stays within a usable range", () => {
    expect(guideMarkerRadius(sphere([0, 0, 0], 0.5), 100)).toBe(50);
    expect(guideMarkerRadius(sphere([0, 0, 0], 0.5), 0.001)).toBe(24);
    expect(guideMarkerRadius(box([0, 0, 0], [2, 4, 1]), 1000)).toBe(420);
  });

  it("keeps the highlight visible without swallowing the whole photo", () => {
    const frameMinSide = 800;
    expect(guideMarkerRadius(sphere([0, 0, 0], 0.01), 1, frameMinSide)).toBeCloseTo(24, 6);
    expect(guideMarkerRadius(sphere([0, 0, 0], 1e6), 1, frameMinSide)).toBeCloseTo(frameMinSide * 0.45, 6);
  });

  it("captions the guide image with the region and what the arrows mean", () => {
    expect(guideCaption(box([0, 0, 0]), 2)).toBe("区域 3 · 盒选 · 箭头 = 补拍机位（箭头指向被补拍区域）");
  });
});

describe("guide zoom", () => {
  it("zooms continuously and never leaves the allowed range", () => {
    expect(clampGuideZoom(2.345)).toBe(2.35);
    expect(clampGuideZoom(0.2)).toBe(GUIDE_ZOOM_MIN);
    expect(clampGuideZoom(99)).toBe(GUIDE_ZOOM_MAX);
    expect(clampGuideZoom(Number.NaN)).toBe(GUIDE_ZOOM_MIN);
  });

  it("steps in and out by a fixed ratio", () => {
    expect(zoomInGuide(1)).toBe(1.25);
    expect(zoomOutGuide(2)).toBe(1.6);
    expect(zoomOutGuide(1)).toBe(1);
    expect(zoomInGuide(GUIDE_ZOOM_MAX)).toBe(GUIDE_ZOOM_MAX);
  });

  it("maps wheel gestures exponentially in both directions", () => {
    expect(zoomFromWheel(2, -100)).toBeGreaterThan(2);
    expect(zoomFromWheel(2, 100)).toBeLessThan(2);
    // At the minimum the gesture simply cannot zoom out any further.
    expect(zoomFromWheel(1, 100)).toBe(GUIDE_ZOOM_MIN);
    expect(zoomFromWheel(1, -1_000_000)).toBe(GUIDE_ZOOM_MAX);
    // Equal gestures produce equal ratios, so zooming feels even at any scale.
    expect(zoomFromWheel(4, -100) / 4).toBeCloseTo(zoomFromWheel(4, -100) / 4, 6);
    expect(zoomFromWheel(4, -100) / 4).toBeCloseTo(zoomFromWheel(2, -100) / 2, 1);
  });

  it("labels the current zoom as a percentage", () => {
    expect(guideZoomLabel(1)).toBe("100%");
    expect(guideZoomLabel(2.5)).toBe("250%");
  });
});
