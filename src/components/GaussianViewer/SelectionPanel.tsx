import type { GaussianCrop } from "../../types/pipeline";
import { NumberField } from "./TransformPanel";

export function SelectionPanel({ crop, kind, onBegin, onChange, onCommit, onEnable }: {
  crop: GaussianCrop;
  kind: "sphere" | "box";
  onBegin: () => void;
  onChange: (crop: Exclude<GaussianCrop, null>) => void;
  onCommit: () => void;
  onEnable: () => void;
}) {
  const activeCrop = crop?.kind === kind ? crop : null;
  const kindLabel = kind === "sphere" ? "球形" : "盒形";

  const centerField = (index: 0 | 1 | 2) => {
    if (!activeCrop) return null;
    const axis = ["X", "Y", "Z"][index];
    return <NumberField key={axis} label={axis} name={`区域位置 ${axis}`} value={activeCrop.center[index]} onBegin={onBegin} onCommit={onCommit} onChange={(value) => {
      const center = [...activeCrop.center] as [number, number, number];
      center[index] = value;
      onChange({ ...activeCrop, center });
    }} />;
  };

  const sizeField = (index: 0 | 1 | 2) => {
    if (activeCrop?.kind !== "box") return null;
    const axis = ["X", "Y", "Z"][index];
    return <NumberField key={axis} label={axis} name={`盒形尺寸 ${axis}`} value={activeCrop.size[index]} mode="scale" onBegin={onBegin} onCommit={onCommit} onChange={(value) => {
      const size = [...activeCrop.size] as [number, number, number];
      size[index] = value;
      onChange({ ...activeCrop, size });
    }} />;
  };

  return <aside className={`transform-panel selection-panel ${activeCrop ? "enabled" : "disabled"}`} aria-label="选择区域">
    <div className="transform-panel-heading">
      <strong>选择区域</strong>
      <small>{kindLabel} · 区域内保留</small>
    </div>
    {activeCrop ? <>
      <section><h4>位置</h4><div className="transform-fields">{centerField(0)}{centerField(1)}{centerField(2)}</div></section>
      <section><h4>{activeCrop.kind === "sphere" ? "半径" : "尺寸"}</h4>{activeCrop.kind === "sphere"
        ? <NumberField label="R" name="球形半径" value={activeCrop.radius} mode="scale" onBegin={onBegin} onCommit={onCommit} onChange={(radius) => onChange({ ...activeCrop, radius })} />
        : <div className="transform-fields">{sizeField(0)}{sizeField(1)}{sizeField(2)}</div>}</section>
    </> : <div className="selection-empty">
      <p>当前未应用裁切。重新启用后，将按完整模型范围创建新的{kindLabel}区域。</p>
      <button type="button" onClick={onEnable}>启用{kindLabel}裁切</button>
    </div>}
  </aside>;
}
