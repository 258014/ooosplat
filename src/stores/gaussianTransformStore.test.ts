import { beforeEach, describe, expect, it } from "vitest";
import { IDENTITY_TRANSFORM, maskBit, setMaskBit, useGaussianTransformStore } from "./gaussianTransformStore";

const loadProject = (splatCount = 16) => useGaussianTransformStore.getState().load({
  projectId: "00000000-0000-0000-0000-000000000001",
  modelPath: "final.ply",
  assetPath: "preview.ply",
  assetUrl: "asset://preview.ply",
  format: "ply",
  fileSize: 1,
  splatCount,
  transform: IDENTITY_TRANSFORM,
  editing: { crop: null, revision: 0, sourceSplatCount: splatCount, deletedCount: 0 },
  editMaskAssetPath: null,
  editMaskAssetUrl: null,
});

describe("GaussianTransformStore", () => {
  beforeEach(() => useGaussianTransformStore.getState().close());

  it("records a drag as one transaction and supports undo and redo", () => {
    const store = useGaussianTransformStore.getState();
    store.beginTransaction();
    store.setTransformLive({ ...IDENTITY_TRANSFORM, position: [1, 2, 3] });
    store.setTransformLive({ ...IDENTITY_TRANSFORM, position: [4, 5, 6] });
    store.commitTransaction();
    expect(useGaussianTransformStore.getState().history).toHaveLength(1);
    useGaussianTransformStore.getState().undo();
    expect(useGaussianTransformStore.getState().transform.position).toEqual([0, 0, 0]);
    useGaussianTransformStore.getState().redo();
    expect(useGaussianTransformStore.getState().transform.position).toEqual([4, 5, 6]);
  });

  it("caps history at 100 committed transforms", () => {
    for (let index = 1; index <= 105; index += 1) {
      useGaussianTransformStore.getState().beginTransaction();
      useGaussianTransformStore.getState().setTransformLive({ ...IDENTITY_TRANSFORM, scale: index });
      useGaussianTransformStore.getState().commitTransaction();
    }
    expect(useGaussianTransformStore.getState().history).toHaveLength(100);
  });

  it("does not overwrite an in-flight save state for a no-op transaction", () => {
    useGaussianTransformStore.getState().setSaveState("saving");
    useGaussianTransformStore.getState().beginTransaction();
    useGaussianTransformStore.getState().commitTransaction();
    expect(useGaussianTransformStore.getState().saveState).toBe("saving");
  });

  it("keeps crop and deletion operations in one chronological undo history", () => {
    loadProject();
    const store = useGaussianTransformStore.getState();
    store.beginCropTransaction();
    store.setCropLive({ kind: "sphere", center: [1, 2, 3], radius: 4 });
    store.commitCropTransaction();
    const selection = new Uint8Array(2);
    setMaskBit(selection, 3, true);
    store.setSelectionMask(selection);
    store.deleteSelection();

    expect(useGaussianTransformStore.getState().editing.deletedCount).toBe(1);
    useGaussianTransformStore.getState().undo();
    expect(useGaussianTransformStore.getState().editing.deletedCount).toBe(0);
    expect(maskBit(useGaussianTransformStore.getState().deletedMask, 3)).toBe(false);
    useGaussianTransformStore.getState().undo();
    expect(useGaussianTransformStore.getState().editing.crop).toBeNull();
    useGaussianTransformStore.getState().redo();
    useGaussianTransformStore.getState().redo();
    expect(useGaussianTransformStore.getState().editing.crop).toEqual({ kind: "sphere", center: [1, 2, 3], radius: 4 });
    expect(maskBit(useGaussianTransformStore.getState().deletedMask, 3)).toBe(true);
  });

  it("does not persist the temporary yellow selection when changing tools", () => {
    loadProject();
    const selection = new Uint8Array(2);
    setMaskBit(selection, 1, true);
    useGaussianTransformStore.getState().setSelectionMask(selection);
    useGaussianTransformStore.getState().setTool("sphere");
    expect(useGaussianTransformStore.getState().selectedCount).toBe(0);
    expect(useGaussianTransformStore.getState().selectionMask).toEqual(new Uint8Array(2));
  });

  it("resets transforms, crop, deletion and history to the original final.ply state", () => {
    loadProject();
    const store = useGaussianTransformStore.getState();
    store.beginTransaction();
    store.setTransformLive({ position: [1, 2, 3], rotation: [4, 5, 6], scale: 2 });
    store.commitTransaction();
    store.beginCropTransaction();
    store.setCropLive({ kind: "sphere", center: [1, 0, 0], radius: 3 });
    store.commitCropTransaction();
    const selection = new Uint8Array(2);
    setMaskBit(selection, 5, true);
    store.setSelectionMask(selection);
    store.deleteSelection();

    useGaussianTransformStore.getState().resetAll();
    const reset = useGaussianTransformStore.getState();
    expect(reset.transform).toEqual(IDENTITY_TRANSFORM);
    expect(reset.editing.crop).toBeNull();
    expect(reset.editing.deletedCount).toBe(0);
    expect(reset.deletedMask).toEqual(new Uint8Array(2));
    expect(reset.history).toHaveLength(0);
    expect(reset.future).toHaveLength(0);
    expect(reset.saveState).toBe("dirty");
  });
});
