// Avatars and group/channel icons: picking, scaling and rendering.

/** Edge length of uploaded icons; the server accepts up to 256 KB. */
const ICON_SIZE = 128;

export const imageUrl = (id) => `/api/images/${id}`;

export function initials(name) {
  return (name || "").split(/\s+/).filter(Boolean).slice(0, 2).map((w) => w[0].toUpperCase()).join("") || "?";
}

/**
 * A round avatar: the image, or the initials on a colored background.
 * `size` is "small" (inline), "medium" (lists) or "large" (profile).
 */
export function avatar(user, size = "small") {
  const name = user?.display_name || user?.name || "?";
  const node = document.createElement("span");
  node.className = `avatar ${size}`;
  if (user?.avatar_id) {
    const img = document.createElement("img");
    img.src = imageUrl(user.avatar_id);
    img.alt = "";
    node.append(img);
  } else {
    node.textContent = initials(name).slice(0, 1);
    node.style.background = `hsl(${hue(user?.name || name)} 45% 45%)`;
  }
  return node;
}

function hue(text) {
  let h = 0;
  for (const c of text) h = (h * 31 + c.codePointAt(0)) % 360;
  return h;
}

/** Lets the user pick an image file; resolves to null when cancelled. */
export function pickImage() {
  return new Promise((resolve) => {
    const input = document.createElement("input");
    input.type = "file";
    input.accept = "image/png,image/jpeg,image/gif,image/webp";
    input.addEventListener("change", () => resolve(input.files[0] || null));
    input.addEventListener("cancel", () => resolve(null));
    input.click();
  });
}

/** Crops the image to a centered square and scales it to a data URL. */
export async function scaleImage(file, size = ICON_SIZE) {
  const bitmap = await createImageBitmap(file).catch(() => {
    throw new Error("This file is not an image the browser can read.");
  });
  const side = Math.min(bitmap.width, bitmap.height);
  const canvas = document.createElement("canvas");
  canvas.width = canvas.height = Math.min(size, side);
  canvas.getContext("2d").drawImage(bitmap,
    (bitmap.width - side) / 2, (bitmap.height - side) / 2, side, side,
    0, 0, canvas.width, canvas.height);
  bitmap.close();
  const webp = canvas.toDataURL("image/webp", 0.9);
  // Browsers without a WebP encoder silently return PNG.
  return webp.startsWith("data:image/webp") ? webp : canvas.toDataURL("image/png");
}

/** Picks, scales and uploads an image with `upload(dataUrl)`; false if cancelled. */
export async function chooseAndUpload(upload) {
  const file = await pickImage();
  if (!file) return false;
  await upload(await scaleImage(file));
  return true;
}
