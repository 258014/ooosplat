export const EDITABLE_SPLAT_ASSET_OPTIONS = Object.freeze({
  data: Object.freeze({ reorder: false }),
});

export function requiredSplatTextureSide(splatCount: number) {
  return Math.ceil(Math.sqrt(Math.max(0, splatCount)));
}

export function splatTextureCapacityError(splatCount: number, maximumTextureSide: number) {
  const requiredTextureSide = requiredSplatTextureSide(splatCount);
  return requiredTextureSide > maximumTextureSide
    ? `当前显卡支持的最大纹理尺寸为 ${maximumTextureSide}，但该模型需要至少 ${requiredTextureSide}。无法安全创建预览资源。`
    : null;
}
