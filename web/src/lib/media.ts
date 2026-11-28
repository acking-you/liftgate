/** Raster formats the gateway keeps as previews; SVG and other scriptable formats never render. */
const RASTER_TYPES = new Set(["image/png", "image/jpeg", "image/jpg", "image/gif", "image/webp"]);
const BASE64_RE = /^[A-Za-z0-9+/=\r\n]+$/;

/** `data:` URL for a captured image preview, or `null` when there is nothing safe to draw. */
export function imageDataUrl(mediaType: string, data: string | null | undefined): string | null {
  if (!data) return null;
  const type = mediaType.trim().toLowerCase();
  if (!RASTER_TYPES.has(type) || !BASE64_RE.test(data)) return null;
  return `data:${type};base64,${data.replace(/[\r\n]/g, "")}`;
}

/** Decoded size of a base64 payload of `base64Bytes` characters. */
export function decodedBytes(base64Bytes: number): number {
  return Math.floor((base64Bytes * 3) / 4);
}

/** File extension for a download name. */
export function imageExtension(mediaType: string): string {
  const sub = mediaType.trim().toLowerCase().split("/")[1] ?? "img";
  return sub === "jpeg" ? "jpg" : sub.replace(/[^a-z0-9]/g, "") || "img";
}
