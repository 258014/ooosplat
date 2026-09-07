import { Color, LAYERID_WORLD, Vec3, type Application } from "playcanvas";
import type { GaussianCrop } from "../../types/pipeline";

const SPHERE_SEGMENTS = 72;

export interface CropOutlineGeometry {
  positions: Vec3[];
  colors: Color[];
}

export function buildCropOutline(crop: Exclude<GaussianCrop, null>): CropOutlineGeometry {
  const positions: Vec3[] = [];
  const colors: Color[] = [];
  const color = new Color(0.18, 0.46, 1, 0.94);
  const addLine = (start: Vec3, end: Vec3) => {
    positions.push(start, end);
    colors.push(color, color);
  };

  if (crop.kind === "box") {
    const [cx, cy, cz] = crop.center;
    const [hx, hy, hz] = crop.size.map((value) => value / 2);
    const corners = [
      new Vec3(cx - hx, cy - hy, cz - hz), new Vec3(cx + hx, cy - hy, cz - hz),
      new Vec3(cx - hx, cy + hy, cz - hz), new Vec3(cx + hx, cy + hy, cz - hz),
      new Vec3(cx - hx, cy - hy, cz + hz), new Vec3(cx + hx, cy - hy, cz + hz),
      new Vec3(cx - hx, cy + hy, cz + hz), new Vec3(cx + hx, cy + hy, cz + hz),
    ];
    for (const [start, end] of [
      [0, 1], [2, 3], [4, 5], [6, 7],
      [0, 2], [1, 3], [4, 6], [5, 7],
      [0, 4], [1, 5], [2, 6], [3, 7],
    ]) addLine(corners[start], corners[end]);
  } else {
    const center = new Vec3(...crop.center);
    for (let plane = 0; plane < 3; plane += 1) {
      for (let index = 0; index < SPHERE_SEGMENTS; index += 1) {
        const point = (angle: number) => {
          const first = Math.cos(angle) * crop.radius;
          const second = Math.sin(angle) * crop.radius;
          if (plane === 0) return new Vec3(center.x + first, center.y + second, center.z);
          if (plane === 1) return new Vec3(center.x + first, center.y, center.z + second);
          return new Vec3(center.x, center.y + first, center.z + second);
        };
        addLine(point(index / SPHERE_SEGMENTS * Math.PI * 2), point((index + 1) / SPHERE_SEGMENTS * Math.PI * 2));
      }
    }
  }

  return { positions, colors };
}

export class CropOutline {
  private geometry: CropOutlineGeometry | null = null;
  private visible = false;
  private destroyed = false;
  private readonly stopDrawing: () => void;

  constructor(app: Application) {
    const worldLayer = app.scene.layers.getLayerById(LAYERID_WORLD) ?? undefined;
    const draw = () => {
      if (!this.visible || !this.geometry) return;
      app.drawLines(this.geometry.positions, this.geometry.colors, false, worldLayer);
    };
    const updateHandle = app.on("update", draw);
    this.stopDrawing = () => updateHandle.off();
  }

  setCrop(crop: GaussianCrop) {
    this.geometry = crop ? buildCropOutline(crop) : null;
  }

  setVisible(visible: boolean) {
    this.visible = visible;
  }

  destroy() {
    if (this.destroyed) return;
    this.destroyed = true;
    this.stopDrawing();
  }
}
