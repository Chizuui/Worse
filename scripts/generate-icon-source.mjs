import { deflateSync } from "node:zlib";
import { mkdirSync, writeFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const SIZE = 1024;
const OUTPUT = resolve(dirname(fileURLToPath(import.meta.url)), "assets", "icon-source.png");
const pixels = new Uint8Array(SIZE * SIZE * 4);

const clamp01 = (value) => Math.min(1, Math.max(0, value));
const mix = (from, to, amount) => from + (to - from) * amount;
const smoothstep = (edge0, edge1, value) => {
  const t = clamp01((value - edge0) / (edge1 - edge0));
  return t * t * (3 - 2 * t);
};

function blend(x, y, red, green, blue, alpha) {
  if (x < 0 || y < 0 || x >= SIZE || y >= SIZE || alpha <= 0) return;
  const offset = (y * SIZE + x) * 4;
  const sourceAlpha = clamp01(alpha);
  const destinationAlpha = pixels[offset + 3] / 255;
  const outputAlpha = sourceAlpha + destinationAlpha * (1 - sourceAlpha);
  if (outputAlpha === 0) return;
  pixels[offset] = Math.round((red * sourceAlpha + pixels[offset] * destinationAlpha * (1 - sourceAlpha)) / outputAlpha);
  pixels[offset + 1] = Math.round((green * sourceAlpha + pixels[offset + 1] * destinationAlpha * (1 - sourceAlpha)) / outputAlpha);
  pixels[offset + 2] = Math.round((blue * sourceAlpha + pixels[offset + 2] * destinationAlpha * (1 - sourceAlpha)) / outputAlpha);
  pixels[offset + 3] = Math.round(outputAlpha * 255);
}

function roundedRectSdf(px, py, cx, cy, halfWidth, halfHeight, radius) {
  const qx = Math.abs(px - cx) - halfWidth + radius;
  const qy = Math.abs(py - cy) - halfHeight + radius;
  return Math.min(Math.max(qx, qy), 0) + Math.hypot(Math.max(qx, 0), Math.max(qy, 0)) - radius;
}

function circleSdf(px, py, cx, cy, radius) {
  return Math.hypot(px - cx, py - cy) - radius;
}

function drawShape(bounds, sdf, colorAt, opacity = 1) {
  const left = Math.max(0, Math.floor(bounds.left));
  const top = Math.max(0, Math.floor(bounds.top));
  const right = Math.min(SIZE - 1, Math.ceil(bounds.right));
  const bottom = Math.min(SIZE - 1, Math.ceil(bounds.bottom));
  for (let y = top; y <= bottom; y += 1) {
    for (let x = left; x <= right; x += 1) {
      const coverage = clamp01(0.5 - sdf(x + 0.5, y + 0.5));
      if (coverage <= 0) continue;
      const color = colorAt(x + 0.5, y + 0.5);
      blend(x, y, color[0], color[1], color[2], coverage * opacity * (color[3] ?? 1));
    }
  }
}

function drawRoundedRect(cx, cy, halfWidth, halfHeight, radius, colorAt, opacity = 1) {
  drawShape(
    {
      left: cx - halfWidth - 2,
      top: cy - halfHeight - 2,
      right: cx + halfWidth + 2,
      bottom: cy + halfHeight + 2,
    },
    (x, y) => roundedRectSdf(x, y, cx, cy, halfWidth, halfHeight, radius),
    colorAt,
    opacity,
  );
}

function drawCircle(cx, cy, radius, colorAt, opacity = 1) {
  drawShape(
    { left: cx - radius - 2, top: cy - radius - 2, right: cx + radius + 2, bottom: cy + radius + 2 },
    (x, y) => circleSdf(x, y, cx, cy, radius),
    colorAt,
    opacity,
  );
}

function drawStroke(cx, cy, halfWidth, halfHeight, radius, colorAt, opacity = 1) {
  drawShape(
    { left: cx - halfWidth - 3, top: cy - halfHeight - 3, right: cx + halfWidth + 3, bottom: cy + halfHeight + 3 },
    (x, y) => Math.abs(roundedRectSdf(x, y, cx, cy, halfWidth, halfHeight, radius) + 1.5) - 2,
    colorAt,
    opacity,
  );
}

const background = (x, y) => {
  const vertical = smoothstep(96, 928, y);
  const glowDistance = Math.hypot(x - 520, y - 470);
  const glow = Math.exp(-(glowDistance * glowDistance) / (2 * 250 * 250));
  return [
    mix(24, 7, vertical) + glow * 34,
    mix(35, 13, vertical) + glow * 23,
    mix(56, 22, vertical) + glow * 8,
    1,
  ];
};

drawRoundedRect(512, 512, 448, 448, 206, background);
drawStroke(512, 512, 448, 448, 206, () => [252, 211, 77, 0.26], 1);
drawRoundedRect(512, 512, 402, 402, 172, () => [255, 255, 255, 0.04], 1);

drawCircle(512, 512, 340, () => [251, 146, 60, 0.07], 1);
drawCircle(512, 512, 284, () => [251, 191, 36, 0.055], 1);

const leftRail = { cx: 354, cy: 512, hw: 57, hh: 252, r: 57 };
const rightRail = { cx: 670, cy: 512, hw: 57, hh: 252, r: 57 };
const bridge = { cx: 512, cy: 512, hw: 158, hh: 57, r: 57 };

const markSdf = (x, y) => Math.min(
  roundedRectSdf(x, y, leftRail.cx, leftRail.cy, leftRail.hw, leftRail.hh, leftRail.r),
  roundedRectSdf(x, y, rightRail.cx, rightRail.cy, rightRail.hw, rightRail.hh, rightRail.r),
  roundedRectSdf(x, y, bridge.cx, bridge.cy, bridge.hw, bridge.hh, bridge.r),
);

for (const [spread, opacity] of [[30, 0.06], [22, 0.09], [14, 0.13], [7, 0.16]]) {
  drawShape(
    { left: 260 - spread, top: 230, right: 770 + spread, bottom: 800 + spread },
    (x, y) => markSdf(x, y) - spread,
    () => [0, 0, 0, opacity],
  );
}

const railColor = (x, y) => {
  const vertical = smoothstep(268, 756, y);
  const edgeLight = 1 - smoothstep(0, 42, Math.abs(x - leftRail.cx));
  return [
    mix(253, 234, vertical) + edgeLight * 5,
    mix(196, 88, vertical),
    mix(48, 12, vertical),
    1,
  ];
};

drawRoundedRect(leftRail.cx, leftRail.cy, leftRail.hw, leftRail.hh, leftRail.r, railColor);
drawRoundedRect(rightRail.cx, rightRail.cy, rightRail.hw, rightRail.hh, rightRail.r, (x, y) => {
  const vertical = smoothstep(268, 756, y);
  const edgeLight = 1 - smoothstep(0, 42, Math.abs(x - rightRail.cx));
  return [
    mix(251, 234, vertical) + edgeLight * 5,
    mix(146, 88, vertical),
    mix(60, 22, vertical),
    1,
  ];
});
drawRoundedRect(bridge.cx, bridge.cy, bridge.hw, bridge.hh, bridge.r, (x, y) => {
  const vertical = smoothstep(466, 558, y);
  return [mix(255, 253, vertical), mix(247, 230, vertical), mix(237, 190, vertical), 1];
});

drawRoundedRect(leftRail.cx - 14, 335, 16, 54, 16, () => [255, 255, 255, 0.16]);
drawRoundedRect(rightRail.cx - 14, 335, 16, 54, 16, () => [255, 255, 255, 0.12]);

drawCircle(512, 512, 19, () => [12, 18, 32, 0.96]);
drawCircle(512, 512, 7, () => [253, 230, 138, 0.95]);

function crc32(buffer) {
  let crc = 0xffffffff;
  for (const byte of buffer) {
    crc ^= byte;
    for (let bit = 0; bit < 8; bit += 1) {
      crc = (crc >>> 1) ^ (0xedb88320 & -(crc & 1));
    }
  }
  return (crc ^ 0xffffffff) >>> 0;
}

function chunk(type, data) {
  const typeBytes = Buffer.from(type, "ascii");
  const length = Buffer.alloc(4);
  length.writeUInt32BE(data.length, 0);
  const checksum = Buffer.alloc(4);
  checksum.writeUInt32BE(crc32(Buffer.concat([typeBytes, data])), 0);
  return Buffer.concat([length, typeBytes, data, checksum]);
}

const header = Buffer.alloc(13);
header.writeUInt32BE(SIZE, 0);
header.writeUInt32BE(SIZE, 4);
header[8] = 8;
header[9] = 6;
header[10] = 0;
header[11] = 0;
header[12] = 0;

const scanlines = Buffer.alloc((SIZE * 4 + 1) * SIZE);
for (let y = 0; y < SIZE; y += 1) {
  const rowStart = y * (SIZE * 4 + 1);
  scanlines[rowStart] = 0;
  scanlines.set(pixels.subarray(y * SIZE * 4, (y + 1) * SIZE * 4), rowStart + 1);
}

const png = Buffer.concat([
  Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
  chunk("IHDR", header),
  chunk("IDAT", deflateSync(scanlines, { level: 9 })),
  chunk("IEND", Buffer.alloc(0)),
]);

mkdirSync(dirname(OUTPUT), { recursive: true });
writeFileSync(OUTPUT, png);
console.log(`Wrote ${OUTPUT} (${SIZE}x${SIZE}, ${png.length} bytes)`);
