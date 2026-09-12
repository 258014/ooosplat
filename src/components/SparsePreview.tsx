import { useEffect, useRef, useState } from "react";
import type { SparsePreview as SparsePreviewData } from "../types/pipeline";

type Mat4 = Float32Array;

function perspective(fovy: number, aspect: number, near: number, far: number): Mat4 {
  const f = 1 / Math.tan(fovy / 2);
  return new Float32Array([
    f / aspect, 0, 0, 0,
    0, f, 0, 0,
    0, 0, (far + near) / (near - far), -1,
    0, 0, (2 * far * near) / (near - far), 0,
  ]);
}

function lookAt(eye: [number, number, number], target: [number, number, number], up: [number, number, number]): Mat4 {
  const [ex, ey, ez] = eye;
  const [tx, ty, tz] = target;
  let zx = ex - tx, zy = ey - ty, zz = ez - tz;
  const zl = Math.hypot(zx, zy, zz) || 1;
  zx /= zl; zy /= zl; zz /= zl;
  let xx = up[1] * zz - up[2] * zy;
  let xy = up[2] * zx - up[0] * zz;
  let xz = up[0] * zy - up[1] * zx;
  const xl = Math.hypot(xx, xy, xz) || 1;
  xx /= xl; xy /= xl; xz /= xl;
  const yx = zy * xz - zz * xy;
  const yy = zz * xx - zx * xz;
  const yz = zx * xy - zy * xx;
  return new Float32Array([
    xx, yx, zx, 0,
    xy, yy, zy, 0,
    xz, yz, zz, 0,
    -(xx * ex + xy * ey + xz * ez),
    -(yx * ex + yy * ey + yz * ez),
    -(zx * ex + zy * ey + zz * ez), 1,
  ]);
}

function multiply(a: Mat4, b: Mat4): Mat4 {
  const out = new Float32Array(16);
  for (let c = 0; c < 4; c++) {
    for (let r = 0; r < 4; r++) {
      out[c * 4 + r] =
        a[0 * 4 + r] * b[c * 4 + 0] +
        a[1 * 4 + r] * b[c * 4 + 1] +
        a[2 * 4 + r] * b[c * 4 + 2] +
        a[3 * 4 + r] * b[c * 4 + 3];
    }
  }
  return out;
}

function compileShader(gl: WebGLRenderingContext, type: number, source: string): WebGLShader | null {
  const shader = gl.createShader(type);
  if (!shader) return null;
  gl.shaderSource(shader, source);
  gl.compileShader(shader);
  return shader;
}

const VERT = `attribute vec3 aPos; attribute vec3 aColor;
uniform mat4 uProj; uniform mat4 uView; uniform float uPointSize;
varying vec3 vColor;
void main(){ gl_Position = uProj * uView * vec4(aPos,1.0); gl_PointSize = uPointSize; vColor = aColor; }`;
const FRAG = `precision mediump float; varying vec3 vColor;
void main(){ vec2 d = gl_PointCoord - vec2(0.5); if(dot(d,d) > 0.25) discard; gl_FragColor = vec4(vColor, 1.0); }`;

export default function SparsePreview({ preview, onClose }: { preview: SparsePreviewData; onClose: () => void }) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const gl = canvas.getContext("webgl", { antialias: false });
    if (!gl) { setError("当前环境不支持 WebGL"); return; }

    const vs = compileShader(gl, gl.VERTEX_SHADER, VERT);
    const fs = compileShader(gl, gl.FRAGMENT_SHADER, FRAG);
    if (!vs || !fs) { setError("着色器编译失败"); return; }
    const prog = gl.createProgram();
    if (!prog) { setError("程序创建失败"); return; }
    gl.attachShader(prog, vs); gl.attachShader(prog, fs);
    gl.bindAttribLocation(prog, 0, "aPos");
    gl.bindAttribLocation(prog, 1, "aColor");
    gl.linkProgram(prog);
    if (!gl.getProgramParameter(prog, gl.LINK_STATUS)) { setError("程序链接失败"); return; }
    gl.useProgram(prog);

    const points = preview.points;
    const cameras = preview.cameras;
    const all = points.length + cameras.length;
    if (all === 0) { setError("没有可预览的点云"); gl.deleteProgram(prog); return; }

    // Bounding box + center
    let minX = Infinity, minY = Infinity, minZ = Infinity;
    let maxX = -Infinity, maxY = -Infinity, maxZ = -Infinity;
    const fill = (p: [number, number, number]) => {
      minX = Math.min(minX, p[0]); maxX = Math.max(maxX, p[0]);
      minY = Math.min(minY, p[1]); maxY = Math.max(maxY, p[1]);
      minZ = Math.min(minZ, p[2]); maxZ = Math.max(maxZ, p[2]);
    };
    points.forEach((p) => fill([p[0], p[1], p[2]])); cameras.forEach((c) => fill(c));
    const cx = (minX + maxX) / 2, cy = (minY + maxY) / 2, cz = (minZ + maxZ) / 2;
    const span = Math.max(maxX - minX, maxY - minY, maxZ - minZ, 1);

    // Interleaved data: point cloud (normalized to [-1,1] around center)
    const norm = (p: [number, number, number]): [number, number, number] => [
      (p[0] - cx) / span, (p[1] - cy) / span, (p[2] - cz) / span,
    ];
    const data: number[] = [];
    points.forEach((p) => { const [x, y, z] = norm([p[0], p[1], p[2]]); data.push(x, y, z, p[3] / 255, p[4] / 255, p[5] / 255); });
    cameras.forEach((c) => { const [x, y, z] = norm([c[0], c[1], c[2]]); data.push(x, y, z, 0.12, 1.0, 0.38); });

    const buf = gl.createBuffer();
    gl.bindBuffer(gl.ARRAY_BUFFER, buf);
    gl.bufferData(gl.ARRAY_BUFFER, new Float32Array(data), gl.STATIC_DRAW);
    const stride = 6 * 4;
    gl.enableVertexAttribArray(0); gl.vertexAttribPointer(0, 3, gl.FLOAT, false, stride, 0);
    gl.enableVertexAttribArray(1); gl.vertexAttribPointer(1, 3, gl.FLOAT, false, stride, 3 * 4);

    const uProj = gl.getUniformLocation(prog, "uProj");
    const uView = gl.getUniformLocation(prog, "uView");
    const uPointSize = gl.getUniformLocation(prog, "uPointSize");
    gl.enable(gl.DEPTH_TEST); gl.enable(gl.BLEND); gl.blendFunc(gl.SRC_ALPHA, gl.ONE_MINUS_SRC_ALPHA);
    gl.clearColor(0.09, 0.1, 0.12, 1);

    let yaw = 0.6, pitch = 0.35, dist = 2.4, pointer = false, lastX = 0, lastY = 0;
    let raf = 0;
    const draw = () => {
      const w = canvas.clientWidth, h = canvas.clientHeight;
      gl.viewport(0, 0, w, h);
      gl.clear(gl.COLOR_BUFFER_BIT | gl.DEPTH_BUFFER_BIT);
      const eye: [number, number, number] = [
        cx + Math.cos(pitch) * Math.sin(yaw) * dist * span,
        cy + Math.sin(pitch) * dist * span,
        cz + Math.cos(pitch) * Math.cos(yaw) * dist * span,
      ];
      const proj = perspective(Math.PI / 3, w / h, 0.01, 100);
      const view = lookAt(eye, [cx, cy, cz], [0, 1, 0]);
      gl.uniformMatrix4fv(uProj, false, proj);
      gl.uniformMatrix4fv(uView, false, view);
      gl.uniform1f(uPointSize, 3.2);
      gl.drawArrays(gl.POINTS, 0, points.length);
      gl.uniform1f(uPointSize, 9.0);
      gl.drawArrays(gl.POINTS, points.length, cameras.length);
    };
    const loop = () => { draw(); raf = requestAnimationFrame(loop); };
    loop();

    const onDown = (e: PointerEvent) => { pointer = true; lastX = e.clientX; lastY = e.clientY; canvas.setPointerCapture(e.pointerId); };
    const onMove = (e: PointerEvent) => { if (!pointer) return; yaw += (e.clientX - lastX) * 0.006; pitch = Math.max(-1.4, Math.min(1.4, pitch + (e.clientY - lastY) * 0.006)); lastX = e.clientX; lastY = e.clientY; };
    const onUp = () => { pointer = false; };
    const onWheel = (e: WheelEvent) => { e.preventDefault(); dist = Math.max(0.6, Math.min(8, dist + e.deltaY * 0.002)); };
    canvas.addEventListener("pointerdown", onDown);
    canvas.addEventListener("pointermove", onMove);
    canvas.addEventListener("pointerup", onUp);
    canvas.addEventListener("wheel", onWheel, { passive: false });

    return () => { cancelAnimationFrame(raf); canvas.removeEventListener("pointerdown", onDown); canvas.removeEventListener("pointermove", onMove); canvas.removeEventListener("pointerup", onUp); canvas.removeEventListener("wheel", onWheel); gl.deleteBuffer(buf); gl.deleteProgram(prog); };
  }, [preview]);

  return (
    <div className="sparse-preview-overlay">
      <div className="sparse-preview-toolbar"><strong>稀疏重建预览</strong><span className="mono">{preview.points.length.toLocaleString()} 点 · {preview.cameras.length} 相机</span><button type="button" onClick={onClose} aria-label="关闭">关闭</button></div>
      <canvas ref={canvasRef} className="sparse-preview-canvas" />
      {error && <div className="sparse-preview-error">{error}</div>}
      <div className="sparse-preview-hint">拖动旋转 · 滚轮缩放 · 绿色为相机位姿</div>
    </div>
  );
}
