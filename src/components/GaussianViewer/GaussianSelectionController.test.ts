import { describe, expect, it, vi } from "vitest";
import { combineSelectionMasks, packSelectionTextureData } from "./GaussianSelectionController";

const processorState = vi.hoisted(() => ({ hits: [] as Uint8Array[] }));

vi.mock("playcanvas", () => ({
  WORKBUFFER_UPDATE_ONCE: "once",
  Mat4: class {
    data = new Float32Array(16);
    mul2() { return this; }
  },
  GSplatProcessor: class {
    private readonly component: { getInstanceTexture: (name: string) => { data: Uint8Array } };
    constructor(_device: unknown, _source: unknown, destination: { component: { getInstanceTexture: (name: string) => { data: Uint8Array } } }) {
      this.component = destination.component;
    }
    setParameter() {}
    process() {
      const hit = processorState.hits.shift();
      if (!hit) throw new Error("Missing processor hit fixture");
      const scratch = this.component.getInstanceTexture("ooosplatScratch");
      scratch.data.fill(0);
      scratch.data.set(hit);
    }
    destroy() {}
  },
}));

function createTexture(size = 8) {
  const data = new Uint8Array(size);
  return {
    width: size,
    height: 1,
    data,
    lock: vi.fn(() => data),
    unlock: vi.fn(),
    read: vi.fn(async () => data.slice()),
  };
}

describe("GaussianSelectionController mask helpers", () => {
  it("packs only splats inside the valid source range", () => {
    const pixels = new Uint8Array([255, 0, 128, 127, 255, 0, 0, 0, 255]);
    expect([...packSelectionTextureData(pixels, 5)]).toEqual([0b00010101]);
  });

  it("supports replace, add, and remove selection composition", () => {
    const current = new Uint8Array([0b00110100]);
    const hit = new Uint8Array([0b01010100]);
    expect([...combineSelectionMasks(current, hit, "replace")]).toEqual([0b01010100]);
    expect([...combineSelectionMasks(current, hit, "add")]).toEqual([0b01110100]);
    expect([...combineSelectionMasks(current, hit, "remove")]).toEqual([0b00100000]);
  });

  it("uploads every consecutive rectangle selection to the yellow selection texture", async () => {
    const { GaussianSelectionController } = await import("./GaussianSelectionController");
    const textures = {
      ooosplatSelected: createTexture(),
      ooosplatDeleted: createTexture(),
      ooosplatScratch: createTexture(),
    };
    const component = {
      entity: { getWorldTransform: () => ({ data: new Float32Array(16) }) },
      getInstanceTexture: (name: keyof typeof textures) => textures[name],
      workBufferUpdate: "auto",
    };
    const camera = { projectionMatrix: {}, camera: { viewMatrix: {} } };
    const requestRender = vi.fn();
    const controller = new GaussianSelectionController(
      {} as never,
      component as never,
      camera as never,
      8,
      requestRender,
    );
    const rectangle = { minX: -1, minY: -1, maxX: 1, maxY: 1 };
    const selections = [
      new Uint8Array([255, 0, 0, 0, 0, 0, 0, 0]),
      new Uint8Array([0, 255, 0, 0, 0, 0, 0, 0]),
      new Uint8Array([0, 0, 255, 0, 0, 0, 0, 0]),
      new Uint8Array([0, 0, 0, 255, 0, 0, 0, 0]),
      new Uint8Array([0, 0, 0, 0, 255, 0, 0, 0]),
    ];

    for (const hit of selections) {
      processorState.hits.push(hit);
      await controller.select(rectangle, "replace", null);
      expect([...textures.ooosplatSelected.data]).toEqual([...hit]);
    }

    expect(textures.ooosplatSelected.unlock).toHaveBeenCalledTimes(5);
    expect(requestRender).toHaveBeenCalledTimes(5);
    controller.destroy();
  });
});
