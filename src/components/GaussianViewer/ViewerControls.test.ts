// @vitest-environment jsdom

import { BoundingBox, PROJECTION_ORTHOGRAPHIC, Vec3, type Entity } from "playcanvas";
import { describe, expect, it, vi } from "vitest";
import { ViewerControls } from "./ViewerControls";

function pointerEvent(type: string, x: number, y: number, button = 0) {
  const event = new MouseEvent(type, { bubbles: true, cancelable: true, button, buttons: type === "pointerup" ? 0 : 1, clientX: x, clientY: y });
  Object.defineProperty(event, "pointerId", { value: 7 });
  return event;
}

function setup() {
  const canvas = document.createElement("canvas");
  const captures = new Set<number>();
  Object.defineProperties(canvas, {
    clientWidth: { value: 1200 },
    clientHeight: { value: 800 },
    setPointerCapture: { value: (id: number) => captures.add(id) },
    releasePointerCapture: { value: (id: number) => captures.delete(id) },
    hasPointerCapture: { value: (id: number) => captures.has(id) },
  });
  const position = new Vec3();
  const setPosition = vi.fn((value: Vec3) => position.copy(value));
  const lookAt = vi.fn();
  const camera: {
    fov: number;
    aspectRatio: number;
    nearClip: number;
    farClip: number;
    projection?: number;
    orthoHeight?: number;
  } = { fov: 52, aspectRatio: 1.5, nearClip: 0.01, farClip: 10_000 };
  const entity = {
    camera,
    setPosition,
    getPosition: () => position,
    lookAt,
    right: new Vec3(1, 0, 0),
    up: new Vec3(0, 1, 0),
  } as unknown as Entity;
  const onOrthographicViewChange = vi.fn();
  const controls = new ViewerControls(canvas, entity, onOrthographicViewChange);
  return { canvas, camera, controls, setPosition, lookAt, onOrthographicViewChange };
}

describe("ViewerControls", () => {
  it("does not move the camera for a click or sub-threshold pointer motion", () => {
    const { canvas, controls, setPosition } = setup();
    const initialUpdates = setPosition.mock.calls.length;

    canvas.dispatchEvent(pointerEvent("pointerdown", 40, 40));
    canvas.dispatchEvent(pointerEvent("pointermove", 41, 41));
    canvas.dispatchEvent(pointerEvent("pointerup", 41, 41));

    expect(setPosition).toHaveBeenCalledTimes(initialUpdates);
    controls.destroy();
  });

  it("cancels an active camera gesture when another control takes ownership", () => {
    const { canvas, controls, setPosition } = setup();
    const initialUpdates = setPosition.mock.calls.length;

    canvas.dispatchEvent(pointerEvent("pointerdown", 20, 20));
    controls.cancelGesture();
    canvas.dispatchEvent(pointerEvent("pointermove", 80, 80));

    expect(setPosition).toHaveBeenCalledTimes(initialUpdates);
    controls.destroy();
  });

  it("limits a single wheel event and can be destroyed repeatedly", () => {
    const { canvas, controls } = setup();
    const before = controls.cameraDistance;

    canvas.dispatchEvent(new WheelEvent("wheel", { bubbles: true, cancelable: true, deltaY: -1_000_000 }));

    expect(controls.cameraDistance).toBeLessThan(before);
    expect(controls.cameraDistance).toBeGreaterThanOrEqual(before * Math.exp(-0.24));
    expect(() => { controls.destroy(); controls.destroy(); }).not.toThrow();
  });

  it("starts above the model and resets to an upright viewing direction", () => {
    const { controls, setPosition } = setup();
    const initialPosition = setPosition.mock.calls.at(-1)?.[0] as Vec3;

    expect(initialPosition.y).toBeGreaterThan(0);
    controls.fit(new BoundingBox(new Vec3(), new Vec3(1, 1, 1)), { resetDirection: true });

    const resetPosition = setPosition.mock.calls.at(-1)?.[0] as Vec3;
    expect(controls.cameraPitch).toBe(22);
    expect(resetPosition.y).toBeGreaterThan(0);
    controls.destroy();
  });

  it("frames tightly and shifts the model away from the transform panel", () => {
    const { controls, lookAt } = setup();
    const bounds = new BoundingBox(new Vec3(), new Vec3(1, 1, 1));
    const oldBoundingSphereDistance = bounds.halfExtents.length() / Math.sin(26 * Math.PI / 180) * 1.18;

    controls.fit(bounds, { resetDirection: true, occupancy: 0.85, rightInsetPx: 286 });

    const viewTarget = lookAt.mock.calls.at(-1)?.[0] as Vec3;
    expect(controls.cameraDistance).toBeLessThan(oldBoundingSphereDistance);
    expect(viewTarget.x).toBeGreaterThan(bounds.center.x);
    controls.destroy();
  });

  it("keeps the full ground grid inside dynamically updated clip planes", () => {
    const { camera, controls } = setup();
    controls.setSceneBounds(new BoundingBox(new Vec3(0, 1, 0), new Vec3(50, 1, 50)));
    controls.fit(new BoundingBox(new Vec3(), new Vec3(1, 1, 1)), { occupancy: 0.85 });

    expect(camera.nearClip).toBeLessThanOrEqual(0.001);
    expect(camera.farClip).toBeGreaterThan(50);
    controls.destroy();
  });

  it("snapshots, orbits, and restores the exact interactive camera state", () => {
    const { controls } = setup();
    const initial = controls.snapshot();

    controls.orbitBy(15);
    expect(controls.snapshot().yaw).toBe(initial.yaw + 15);

    controls.restore(initial);
    expect(controls.snapshot()).toEqual(initial);
    controls.destroy();
  });

  it("reports the current Y-axis orbit center, radius, height, and direction", () => {
    const { controls } = setup();
    controls.fit(new BoundingBox(new Vec3(3, 4, 5), new Vec3(2, 1, 2)), { occupancy: 0.85 });
    const initial = controls.orbitGuideState();

    expect(initial.target).toEqual([3, 4, 5]);
    expect(initial.radius).toBeGreaterThan(0);
    expect(initial.height).toBeGreaterThan(initial.target[1]);

    controls.orbitBy(15);
    const rotated = controls.orbitGuideState();
    expect(rotated.radius).toBeCloseTo(initial.radius);
    expect(rotated.height).toBeCloseTo(initial.height);
    expect(rotated.angleDegrees).toBeCloseTo(initial.angleDegrees + 15);
    controls.destroy();
  });

  it("animates between orthographic editing views", () => {
    const frames: FrameRequestCallback[] = [];
    const request = vi.spyOn(window, "requestAnimationFrame").mockImplementation((callback) => {
      frames.push(callback);
      return frames.length;
    });
    const cancel = vi.spyOn(window, "cancelAnimationFrame").mockImplementation(() => undefined);
    const { camera, controls, setPosition } = setup();
    const before = setPosition.mock.calls.length;

    controls.alignOrthographic("side", new BoundingBox(new Vec3(2, 3, 4), new Vec3(5, 6, 7)));
    expect(camera.projection).toBe(PROJECTION_ORTHOGRAPHIC);
    expect(frames).toHaveLength(1);
    frames.shift()?.(performance.now() + 160);
    expect(setPosition.mock.calls.length).toBeGreaterThan(before);
    expect(frames).toHaveLength(1);
    frames.shift()?.(performance.now() + 1_000);

    controls.destroy();
    request.mockRestore();
    cancel.mockRestore();
  });

  it("returns to perspective and clears the selected view when orbiting", () => {
    const { canvas, camera, controls, onOrthographicViewChange } = setup();
    controls.alignOrthographic("front", new BoundingBox(new Vec3(), new Vec3(2, 1, 3)), 0.85, false);
    expect(camera.projection).toBe(PROJECTION_ORTHOGRAPHIC);
    expect(onOrthographicViewChange).toHaveBeenLastCalledWith("front");

    canvas.dispatchEvent(pointerEvent("pointerdown", 40, 40));
    canvas.dispatchEvent(pointerEvent("pointermove", 60, 48));
    canvas.dispatchEvent(pointerEvent("pointerup", 60, 48));

    expect(camera.projection).not.toBe(PROJECTION_ORTHOGRAPHIC);
    expect(onOrthographicViewChange).toHaveBeenLastCalledWith(null);
    controls.destroy();
  });

});
