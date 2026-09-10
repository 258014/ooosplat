import { useCallback, useEffect, useRef, useState } from "react";
import {
  clampGuideZoom,
  GUIDE_ZOOM_MAX,
  GUIDE_ZOOM_MIN,
  guideZoomLabel,
  zoomFromWheel,
  zoomInGuide,
  zoomOutGuide,
} from "./ReshootGuidance";

/**
 * Shows a reshoot guide picture at its natural size and lets the user zoom it
 * continuously with the wheel, the slider, or the buttons.
 */
export function ZoomableGuideFigure({ src, alt, caption }: { src: string; alt: string; caption: string }) {
  const [zoom, setZoom] = useState(GUIDE_ZOOM_MIN);
  const [open, setOpen] = useState(false);
  const viewRef = useRef<HTMLDivElement | null>(null);

  const zoomBy = useCallback((deltaY: number) => {
    setZoom((current) => zoomFromWheel(current, deltaY));
  }, []);

  // A native listener keeps the wheel non-passive, so zooming never scrolls the panel.
  useEffect(() => {
    const node = viewRef.current;
    if (!node) return;
    const onWheel = (event: WheelEvent) => {
      event.preventDefault();
      zoomBy(event.deltaY);
    };
    node.addEventListener("wheel", onWheel, { passive: false });
    return () => node.removeEventListener("wheel", onWheel);
  }, [zoomBy]);

  return <figure className={open ? "reshoot-guide-figure open" : "reshoot-guide-figure"}>
    <div className="reshoot-guide-figure-view" ref={viewRef}>
      <img src={src} alt={alt} style={{ width: `${zoom * 100}%`, maxWidth: "none" }} />
    </div>
    <div className="reshoot-guide-zoom">
      <button type="button" aria-label="缩小指引图" disabled={zoom <= GUIDE_ZOOM_MIN} onClick={() => setZoom(zoomOutGuide)}>−</button>
      <input
        type="range"
        aria-label="指引图缩放"
        min={GUIDE_ZOOM_MIN}
        max={GUIDE_ZOOM_MAX}
        step={0.01}
        value={zoom}
        onChange={(event) => setZoom(clampGuideZoom(Number(event.target.value)))}
      />
      <button type="button" aria-label="放大指引图" disabled={zoom >= GUIDE_ZOOM_MAX} onClick={() => setZoom(zoomInGuide)}>+</button>
      <span className="mono">{guideZoomLabel(zoom)}</span>
      <button type="button" className="reshoot-guide-expand" aria-expanded={open} onClick={() => setOpen((value) => !value)}>{open ? "收起" : "放大查看"}</button>
    </div>
    <figcaption>{caption}</figcaption>
  </figure>;
}
