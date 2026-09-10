// @vitest-environment jsdom

import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { ZoomableGuideFigure } from "./ZoomableGuideFigure";

describe("ZoomableGuideFigure", () => {
  let container: HTMLDivElement;
  let root: Root;

  beforeEach(() => {
    (globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
    act(() => {
      root.render(<ZoomableGuideFigure src="data:image/png;base64,AAAA" alt="区域 1 补拍方位指引" caption="箭头 = 补拍机位" />);
    });
  });

  afterEach(() => {
    act(() => root.unmount());
    container.remove();
  });

  const image = () => container.querySelector("img") as HTMLImageElement;
  const slider = () => container.querySelector('input[type="range"]') as HTMLInputElement;
  const button = (label: string) => [...container.querySelectorAll("button")].find((item) => item.getAttribute("aria-label") === label) as HTMLButtonElement;

  const setSlider = (value: string) => {
    act(() => {
      // React tracks the last value it saw, so the native setter must be used
      // before dispatching the input event it listens for.
      const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")?.set;
      setter?.call(slider(), value);
      slider().dispatchEvent(new Event("input", { bubbles: true }));
    });
  };

  it("shows the picture at its natural size with the zoom control", () => {
    expect(image().style.width).toBe("100%");
    expect(slider().value).toBe("1");
    expect(container.textContent).toContain("100%");
    expect(button("缩小指引图").disabled).toBe(true);
  });

  it("zooms in continuously through the slider", () => {
    setSlider("3.37");
    expect(image().style.width).toBe("337%");
    expect(container.textContent).toContain("337%");
  });

  it("zooms with the buttons and honours the limits", () => {
    act(() => button("放大指引图").dispatchEvent(new MouseEvent("click", { bubbles: true })));
    expect(image().style.width).toBe("125%");

    act(() => button("缩小指引图").dispatchEvent(new MouseEvent("click", { bubbles: true })));
    expect(image().style.width).toBe("100%");
    expect(button("缩小指引图").disabled).toBe(true);

    setSlider("8");
    expect(image().style.width).toBe("800%");
    expect(button("放大指引图").disabled).toBe(true);
  });

  it("zooms with the mouse wheel without scrolling the panel", () => {
    const view = container.querySelector(".reshoot-guide-figure-view") as HTMLDivElement;
    const wheelUp = new WheelEvent("wheel", { deltaY: -200, bubbles: true, cancelable: true });
    act(() => { view.dispatchEvent(wheelUp); });

    expect(wheelUp.defaultPrevented).toBe(true);
    expect(Number(slider().value)).toBeGreaterThan(1);
    expect(image().style.width).not.toBe("100%");

    const zoomed = Number(slider().value);
    act(() => { view.dispatchEvent(new WheelEvent("wheel", { deltaY: 400, bubbles: true, cancelable: true })); });
    expect(Number(slider().value)).toBeLessThan(zoomed);
    expect(Number(slider().value)).toBe(1);
  });

  it("expands the visible area for a closer look", () => {
    const expand = [...container.querySelectorAll("button")].find((item) => item.textContent === "放大查看") as HTMLButtonElement;
    expect(container.querySelector(".reshoot-guide-figure")?.className).not.toContain("open");

    act(() => expand.dispatchEvent(new MouseEvent("click", { bubbles: true })));
    expect(container.querySelector(".reshoot-guide-figure")?.className).toContain("open");
    expect([...container.querySelectorAll("button")].some((item) => item.textContent === "收起")).toBe(true);
  });
});
