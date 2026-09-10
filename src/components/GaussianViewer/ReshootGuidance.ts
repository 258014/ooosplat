import type { GaussianCrop } from "../../types/pipeline";

export type ReshootRegion = Exclude<GaussianCrop, null>;

/** Two selections closer than this describe the same spot, not a new region. */
const REGION_PRECISION = 3;

export interface ReshootEntry {
  region: ReshootRegion;
  guidance: string;
  /** Annotated screenshot: the circled area plus arrows showing where to stand. */
  guideImage: string | null;
}

export interface GuideMarker {
  /** Screen position of the region centre in image pixels. */
  x: number;
  y: number;
  radius: number;
}

export interface GuideArrow {
  fromX: number;
  fromY: number;
  toX: number;
  toY: number;
  label: string;
  /** Where the label sits relative to the arrow tail. */
  labelX: number;
  labelY: number;
}

export interface GuideLabel {
  text: string;
  x: number;
  y: number;
}

const round = (value: number) => Math.round(value * 10 ** REGION_PRECISION) / 10 ** REGION_PRECISION;

const triple = (values: [number, number, number]) => values.map(round).join(",");

/** Stable identity for a selection, so an identical region cannot be added twice. */
export function regionKey(region: ReshootRegion): string {
  return region.kind === "sphere"
    ? `sphere:${triple(region.center)}:${round(region.radius)}`
    : `box:${triple(region.center)}:${triple(region.size)}`;
}

export function isSameReshootRegion(left: ReshootRegion, right: ReshootRegion): boolean {
  return regionKey(left) === regionKey(right);
}

export function findReshootRegion(regions: ReshootRegion[], region: ReshootRegion): number {
  const key = regionKey(region);
  return regions.findIndex((item) => regionKey(item) === key);
}

const format = (value: number) => value.toFixed(2);

export function regionGeometryLabel(region: ReshootRegion): string {
  const center = `中心 (${region.center.map(format).join(", ")})`;
  return region.kind === "sphere"
    ? `球选 · ${center} · 半径 ${format(region.radius)}`
    : `盒选 · ${center} · 尺寸 (${region.size.map(format).join(", ")})`;
}

/**
 * Guidance must never read the same for two different regions, so every line
 * carries the selection geometry that tells the shooter which spot it means.
 */
export function reshootGuidance(region: ReshootRegion, index: number): string {
  const geometry = regionGeometryLabel(region);
  return region.kind === "sphere"
    ? `区域 ${index + 1}｜${geometry}｜绕该区域缓慢环拍两周：第一周低角度、第二周抬高约 30°，每圈至少 12 个机位，始终让该区域位于画面中央并保留前景与背景两层视差。`
    : `区域 ${index + 1}｜${geometry}｜从正面、左侧、右侧与上方各补拍一组高清画面，相邻机位间隔约 30°；避免仅原地变焦或只沿一个方向平移。`;
}

export function reshootRegionsGuidance(regions: ReshootRegion[]): string[] {
  return regions.map((region, index) => reshootGuidance(region, index));
}

/** Shooting positions the arrows point from, described in the guide image. */
export function shootingDirections(region: ReshootRegion): Array<{ angleDeg: number; label: string }> {
  if (region.kind === "sphere") {
    return [
      { angleDeg: -90, label: "正前低角度" },
      { angleDeg: -45, label: "右前低角度" },
      { angleDeg: 0, label: "右侧低角度" },
      { angleDeg: 45, label: "右后低角度" },
      { angleDeg: 90, label: "正后低角度" },
      { angleDeg: 135, label: "左后低角度" },
      { angleDeg: 180, label: "左侧低角度" },
      { angleDeg: -135, label: "左前低角度" },
      { angleDeg: -90, label: "正前抬高 30°" },
      { angleDeg: 0, label: "右侧抬高 30°" },
      { angleDeg: 90, label: "正后抬高 30°" },
      { angleDeg: 180, label: "左侧抬高 30°" },
    ];
  }
  return [
    { angleDeg: -90, label: "正面" },
    { angleDeg: 180, label: "左侧" },
    { angleDeg: 0, label: "右侧" },
    { angleDeg: -45, label: "右前 30°" },
    { angleDeg: -135, label: "左前 30°" },
  ];
}

/**
 * Places an arrow for every shooting direction on a ring around the circled
 * area. Arrows point inward, so the tail marks where the camera should stand.
 */
export function reshootGuideArrows(marker: GuideMarker, directions: Array<{ angleDeg: number; label: string }>, arrowLength = 46): GuideArrow[] {
  const ringRadius = Math.max(marker.radius + 18, 42);
  return directions.map(({ angleDeg, label }) => {
    const radians = (angleDeg * Math.PI) / 180;
    const dirX = Math.cos(radians);
    const dirY = Math.sin(radians);
    const tailDistance = ringRadius + arrowLength;
    return {
      fromX: marker.x + dirX * tailDistance,
      fromY: marker.y + dirY * tailDistance,
      toX: marker.x + dirX * ringRadius,
      toY: marker.y + dirY * ringRadius,
      label,
      labelX: marker.x + dirX * (tailDistance + 10),
      labelY: marker.y + dirY * (tailDistance + 10),
    };
  });
}

/**
 * Radius the highlighted circle uses, in capture pixels. The guide keeps the
 * render as a photo, so the highlight only needs to stay visible: it is never
 * smaller than 3% of the frame and never larger than 45% of it.
 */
export function guideMarkerRadius(region: ReshootRegion, pixelsPerUnit: number, frameMinSide = 0): number {
  const base = region.kind === "sphere" ? region.radius : Math.max(...region.size) / 2;
  const minimum = frameMinSide > 0 ? frameMinSide * 0.03 : 24;
  const maximum = frameMinSide > 0 ? frameMinSide * 0.45 : 420;
  return Math.max(minimum, Math.min(maximum, Math.abs(base) * pixelsPerUnit));
}

/** Guide pictures are shown at their natural size at 100%. */
export const GUIDE_ZOOM_MIN = 1;
export const GUIDE_ZOOM_MAX = 8;
const GUIDE_ZOOM_STEP = 1.25;

/** Continuous zoom, so a wheel gesture or slider never jumps between fixed steps. */
export function clampGuideZoom(value: number): number {
  if (Number.isNaN(value)) return GUIDE_ZOOM_MIN;
  return Math.round(Math.min(GUIDE_ZOOM_MAX, Math.max(GUIDE_ZOOM_MIN, value)) * 100) / 100;
}

export function zoomInGuide(current: number): number {
  return clampGuideZoom(current * GUIDE_ZOOM_STEP);
}

export function zoomOutGuide(current: number): number {
  return clampGuideZoom(current / GUIDE_ZOOM_STEP);
}

/** Wheel deltas map exponentially, so zooming feels even at every scale. */
export function zoomFromWheel(current: number, deltaY: number): number {
  return clampGuideZoom(current * Math.exp(-deltaY * 0.0015));
}

export function guideZoomLabel(zoom: number): string {
  return `${Math.round(zoom * 100)}%`;
}

export function guideCaption(region: ReshootRegion, index: number): string {
  return `区域 ${index + 1} · ${region.kind === "sphere" ? "球选" : "盒选"} · 箭头 = 补拍机位（箭头指向被补拍区域）`;
}

/**
 * Draws the reshoot marks over a captured render. The picture stays a plain
 * photo of the project: no crop, no dimming, only a thin highlight and arrows.
 */
export function drawReshootGuide(
  context: CanvasRenderingContext2D,
  options: {
    width: number;
    height: number;
    marker: GuideMarker;
    region: ReshootRegion;
    index: number;
    directions: Array<{ angleDeg: number; label: string }>;
  },
): void {
  const { width, height, marker, region, index, directions } = options;
  const arrows = reshootGuideArrows(marker, directions);
  const unit = Math.max(width, height) / 1600;
  const fontSize = Math.max(13, Math.round(13 * unit));
  const captionSize = Math.max(15, Math.round(16 * unit));

  context.save();
  context.lineWidth = Math.max(2, 2.5 * unit);
  context.strokeStyle = "rgba(255, 209, 102, .95)";
  context.fillStyle = "rgba(255, 209, 102, .12)";
  context.beginPath();
  context.arc(marker.x, marker.y, marker.radius, 0, Math.PI * 2);
  context.fill();
  context.stroke();

  context.strokeStyle = "rgba(255, 93, 115, .92)";
  context.fillStyle = "rgba(255, 93, 115, .92)";
  for (const arrow of arrows) {
    const angle = Math.atan2(arrow.toY - arrow.fromY, arrow.toX - arrow.fromX);
    const headLength = Math.max(10, 12 * unit);
    context.beginPath();
    context.moveTo(arrow.fromX, arrow.fromY);
    context.lineTo(arrow.toX, arrow.toY);
    context.stroke();
    context.beginPath();
    context.moveTo(arrow.toX, arrow.toY);
    context.lineTo(
      arrow.toX - headLength * Math.cos(angle - Math.PI / 7),
      arrow.toY - headLength * Math.sin(angle - Math.PI / 7),
    );
    context.lineTo(
      arrow.toX - headLength * Math.cos(angle + Math.PI / 7),
      arrow.toY - headLength * Math.sin(angle + Math.PI / 7),
    );
    context.closePath();
    context.fill();
  }

  context.font = `600 ${fontSize}px 'Microsoft YaHei UI', 'Segoe UI', sans-serif`;
  context.textAlign = "center";
  context.textBaseline = "middle";
  context.fillStyle = "#ffffff";
  context.strokeStyle = "rgba(0, 0, 0, .72)";
  context.lineWidth = Math.max(3, 4 * unit);
  for (const arrow of arrows) {
    // Keep a label inside the picture even when its arrow touches an edge.
    const halfWidth = context.measureText(arrow.label).width / 2 + 6;
    const labelX = Math.min(Math.max(arrow.labelX, halfWidth), width - halfWidth);
    const labelY = Math.min(Math.max(arrow.labelY, fontSize), height - captionSize * 1.6);
    context.strokeText(arrow.label, labelX, labelY);
    context.fillText(arrow.label, labelX, labelY);
  }

  const caption = guideCaption(region, index);
  const barHeight = captionSize * 2.1;
  context.fillStyle = "rgba(8, 12, 20, .62)";
  context.fillRect(0, height - barHeight, width, barHeight);
  context.textAlign = "left";
  context.textBaseline = "middle";
  context.font = `700 ${captionSize}px 'Microsoft YaHei UI', 'Segoe UI', sans-serif`;
  context.fillStyle = "#ffffff";
  context.fillText(caption, 16, height - barHeight / 2);
  context.restore();
}

/**
 * Composes the guide image from a captured render and returns a PNG data URL.
 * The render is kept as-is: the marks are an overlay, not a redrawn scene, so
 * the picture still reads as a photo of the project.
 */
export function composeReshootGuideImage(options: {
  frame: { width: number; height: number; rgba: Uint8ClampedArray | Uint8Array };
  marker: GuideMarker;
  region: ReshootRegion;
  index: number;
  directions: Array<{ angleDeg: number; label: string }>;
}): string | null {
  const { frame, marker, region, index, directions } = options;
  if (frame.width <= 0 || frame.height <= 0) return null;

  const canvas = document.createElement("canvas");
  canvas.width = frame.width;
  canvas.height = frame.height;
  const context = canvas.getContext("2d");
  if (!context) return null;
  const image = context.createImageData(frame.width, frame.height);
  image.data.set(frame.rgba);
  context.putImageData(image, 0, 0);
  drawReshootGuide(context, { width: frame.width, height: frame.height, marker, region, index, directions });
  return canvas.toDataURL("image/png");
}
