// Generates apps/harness-slint/ui/brand/ascii_art.slint: the unicode / ASCII art frames the UI animates
// (block wordmark with a travelling highlight, the ASCII bunny, the dither field, spinners).
// Deterministic (seeded) and dependency-free: `node scripts/ui/gen-ascii-art.mjs`.
import { writeFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const OUT = resolve(dirname(fileURLToPath(import.meta.url)), "../../apps/harness-slint/ui/brand/ascii_art.slint");

// ---- helpers ---------------------------------------------------------------------------------------------
function hash(x, y, s) {
  let h = (x * 374761393 + y * 668265263 + s * 2147483647) | 0;
  h = Math.imul(h ^ (h >>> 13), 1274126177);
  h ^= h >>> 16;
  return (h >>> 0) / 4294967296;
}
const lerp = (a, b, t) => a + (b - a) * t;
const smooth = (t) => t * t * (3 - 2 * t);
// Value noise, smooth in x / y, seeded.
function noise(x, y, s) {
  const xi = Math.floor(x);
  const yi = Math.floor(y);
  const xf = smooth(x - xi);
  const yf = smooth(y - yi);
  return lerp(lerp(hash(xi, yi, s), hash(xi + 1, yi, s), xf), lerp(hash(xi, yi + 1, s), hash(xi + 1, yi + 1, s), xf), yf);
}
const slintString = (s) => `"${s.replace(/\\/g, "\\\\").replace(/"/g, '\\"').replace(/\n/g, "\\n")}"`;
const slintArray = (frames) => `[\n${frames.map((f) => `        ${slintString(f)},`).join("\n")}\n    ]`;

// ---- 1. block wordmark ------------------------------------------------------------------------------------
// 5x7 pixel capitals; every font pixel becomes two characters so cells come out roughly square.
const FONT = {
  W: ["#...#", "#...#", "#...#", "#.#.#", "#.#.#", "##.##", "#...#"],
  O: [".###.", "#...#", "#...#", "#...#", "#...#", "#...#", ".###."],
  R: ["####.", "#...#", "#...#", "####.", "#.#..", "#..#.", "#...#"],
  S: [".####", "#....", "#....", ".###.", "....#", "....#", "####."],
  E: ["#####", "#....", "#....", "####.", "#....", "#....", "#####"],
  H: ["#...#", "#...#", "#...#", "#####", "#...#", "#...#", "#...#"],
  A: [".###.", "#...#", "#...#", "#####", "#...#", "#...#", "#...#"],
  N: ["#...#", "##..#", "##..#", "#.#.#", "#..##", "#..##", "#...#"],
  " ": [".", ".", ".", ".", ".", ".", "."],
};
// `lines` are stacked rows of capitals. One letter at a time is solid (lit); the lit letter walks through
// the whole wordmark and the others are dithered, denser the closer they are to it.
function wordmarkFrames(lines, frames) {
  const rows = lines.map((text) => {
    const cols = [];
    [...text].forEach((ch, i) => {
      const g = FONT[ch];
      for (let x = 0; x < g[0].length; x += 1) cols.push({ li: i, bits: g.map((row) => row[x] === "#") });
      if (i < text.length - 1) cols.push({ li: -1, bits: g.map(() => false) });
    });
    return cols;
  });
  const offsets = lines.map((_, r) => lines.slice(0, r).reduce((n, t) => n + t.length, 0));
  const total = lines.reduce((n, t) => n + t.length, 0);
  const out = [];
  for (let f = 0; f < frames; f += 1) {
    const lit = Math.floor((f / frames) * total) % total;
    const jitter = Math.floor(f / 3);
    const parts = [];
    rows.forEach((cols, r) => {
      for (let y = 0; y < 7; y += 1) {
        let line = "";
        cols.forEach((c, x) => {
          if (!c.bits[y]) {
            line += "  ";
            return;
          }
          const index = offsets[r] + c.li;
          if (index === lit) {
            line += "██";
            return;
          }
          const dist = Math.abs(index - lit);
          const near = Math.max(0, 1 - Math.min(dist, total - dist) / 4);
          const v = hash(x + r * 97, y, 11 + jitter) * 0.6 + near * 0.4;
          line += v > 0.78 ? "▓▓" : v > 0.56 ? "▒▒" : v > 0.36 ? "░░" : v > 0.18 ? "##" : "%%";
        });
        parts.push(line.replace(/\s+$/, ""));
      }
      if (r < rows.length - 1) parts.push("");
    });
    out.push(parts.join("\n"));
  }
  return out;
}

// ---- 2. the ASCII bunny ----------------------------------------------------------------------------------
// Deadpan by default; every few seconds an ear flicks or it gives a side-eye.
const B = {
  idle: ["(\\_/)", "( -.-)", 'o_(")(")'],
  ear1: ["(\\_|)", "( -.-)", 'o_(")(")'],
  ear2: ["(\\_\\)", "( -.-)", 'o_(")(")'],
  peek: ["(\\_/)", "( -.o)", 'o_(")(")'],
  wide: ["(\\_/)", "( o.o)", 'o_(")(")'],
  zz1: ["(\\_/)  z", "( -.-)", 'o_(")(")'],
  zz2: ["(\\_/) zZ", "( -.-)", 'o_(")(")'],
  zz3: ["(\\_/)zZz", "( -.-)", 'o_(")(")'],
  nom1: ["(\\_/)", "( -.-)>", 'o_(")(")'],
  nom2: ["(\\_/)", "( -x-)>", 'o_(")(")'],
};
const rep = (k, n) => Array.from({ length: n }, () => B[k].join("\n"));
const bunnyIdle = [...rep("idle", 14), ...rep("ear1", 1), ...rep("ear2", 2), ...rep("ear1", 1), ...rep("idle", 12), ...rep("peek", 4), ...rep("idle", 10), ...rep("wide", 1), ...rep("idle", 3)];
const bunnySleep = [...rep("idle", 4), ...rep("zz1", 3), ...rep("zz2", 3), ...rep("zz3", 3), ...rep("idle", 3)];
const bunnyWork = [...rep("nom1", 2), ...rep("nom2", 2)];

// ---- 3. dither field -------------------------------------------------------------------------------------
// Sparse glyph grid (drawn with wide letter spacing), dense at the window edge and fading out towards the
// content column. `mirror` flips it for the right edge. Frames drift slowly so cycling them reads as a shimmer.
function fieldFrames(cols, rows, frames, mirror, seed) {
  const out = [];
  for (let f = 0; f < frames; f += 1) {
    const t = (f / frames) * Math.PI * 2;
    let s = "";
    for (let y = 0; y < rows; y += 1) {
      let line = "";
      for (let x = 0; x < cols; x += 1) {
        const edge = mirror ? x / (cols - 1) : 1 - x / (cols - 1);
        // Tall, streaky clusters: noise is stretched vertically, and drifts on a small loop.
        const n = noise(x * 0.42 + Math.cos(t) * 0.35, y * 0.13 + Math.sin(t) * 0.5, seed) * 0.62 + noise(x * 1.3, y * 0.55 + Math.sin(t) * 0.3, seed + 7) * 0.38;
        const v = n * (0.35 + 0.65 * edge * edge);
        line += v > 0.58 ? "#" : v > 0.5 ? "%" : v > 0.43 ? "*" : v > 0.37 ? ":" : " ";
      }
      s += (mirror ? line : line.replace(/\s+$/, "")) + (y < rows - 1 ? "\n" : "");
    }
    out.push(s);
  }
  return out;
}

// ---- 4. spinners ------------------------------------------------------------------------------------------
const spinner = ["▖", "▘", "▝", "▗"];
const scan = Array.from({ length: 12 }, (_, f) => {
  const ramp = " ░▒▓█▓▒░ ";
  let s = "";
  for (let i = 0; i < 8; i += 1) s += ramp[Math.max(0, Math.min(ramp.length - 1, i - f + 6))] ?? " ";
  return s;
});
const bars = Array.from({ length: 8 }, (_, f) => {
  const glyphs = "▁▂▃▄▅▆▇";
  let s = "";
  for (let i = 0; i < 5; i += 1) s += glyphs[Math.round((Math.sin(((f + i * 1.6) / 8) * Math.PI * 2) * 0.5 + 0.5) * (glyphs.length - 1))];
  return s;
});

const body = `// Generated by scripts/ui/gen-ascii-art.mjs. Do not edit by hand.
// Unicode / ASCII art frames: block wordmark, the ASCII bunny, the dither field and the glyph spinners.
export global AsciiArt {
    // "WORSE" over "HARNESS" in 5x7 block capitals; the solid letter walks through the wordmark.
    out property <[string]> wordmark: ${slintArray(wordmarkFrames(["WORSE", "HARNESS"], 48))};
    out property <[string]> wordmark-short: ${slintArray(wordmarkFrames(["HARNESS"], 28))};
    out property <[string]> bunny: ${slintArray(bunnyIdle)};
    out property <[string]> bunny-sleep: ${slintArray(bunnySleep)};
    out property <[string]> bunny-work: ${slintArray(bunnyWork)};
    out property <[string]> field-left: ${slintArray(fieldFrames(46, 108, 6, false, 3))};
    out property <[string]> field-right: ${slintArray(fieldFrames(46, 108, 6, true, 19))};
    out property <[string]> spinner: ${slintArray(spinner)};
    out property <[string]> scan: ${slintArray(scan)};
    out property <[string]> bars: ${slintArray(bars)};
}
`;
writeFileSync(OUT, body);
console.log(`wrote ${OUT} (${body.length} bytes)`);
