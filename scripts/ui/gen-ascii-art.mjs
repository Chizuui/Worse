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
const slintArray2 = (lists) => `[\n${lists.map((frames) => `        [\n${frames.map((f) => `            ${slintString(f)},`).join("\n")}\n        ],`).join("\n")}\n    ]`;

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

// The same wordmark split into one frame list per letter (5x7 cells, two characters per cell, no gap), so a
// letter whose cells did not change between two frames is not redrawn. Letters run left to right, top row first.
function wordmarkLetterFrames(lines, frames) {
  const total = lines.reduce((n, t) => n + t.length, 0);
  const letters = [];
  lines.forEach((text, r) => {
    const offset = lines.slice(0, r).reduce((n, t) => n + t.length, 0);
    [...text].forEach((ch, li) => letters.push({ g: FONT[ch], r, li, index: offset + li }));
  });
  return letters.map(({ g, r, li, index }) => {
    const out = [];
    for (let f = 0; f < frames; f += 1) {
      const lit = Math.floor((f / frames) * total) % total;
      const jitter = Math.floor(f / 3);
      const rows = [];
      for (let y = 0; y < 7; y += 1) {
        let line = "";
        for (let cx = 0; cx < 5; cx += 1) {
          if (g[y][cx] !== "#") {
            line += "  ";
            continue;
          }
          if (index === lit) {
            line += "██";
            continue;
          }
          // Same column index and hash as wordmarkFrames, so the art is identical cell for cell.
          const x = li * 6 + cx;
          const dist = Math.abs(index - lit);
          const near = Math.max(0, 1 - Math.min(dist, total - dist) / 4);
          const v = hash(x + r * 97, y, 11 + jitter) * 0.6 + near * 0.4;
          line += v > 0.78 ? "▓▓" : v > 0.56 ? "▒▒" : v > 0.36 ? "░░" : v > 0.18 ? "##" : "%%";
        }
        rows.push(line);
      }
      out.push(rows.join("\n"));
    }
    return out;
  });
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
// `keep` columns are emitted (the ones nearest the window edge; the rest are always clipped), and rows are
// split into `bands` stacked text items so bands below the window are never laid out. Returns [band][frame].
function fieldBands(cols, keep, rows, bands, frames, mirror, seed) {
  const full = fieldFrames(cols, rows, frames, mirror, seed);
  const per = Math.ceil(rows / bands);
  const out = [];
  for (let b = 0; b < bands; b += 1) {
    out.push(
      full.map((frame) =>
        frame
          .split("\n")
          .slice(b * per, (b + 1) * per)
          // Never an empty line: a band must keep its full height so the bands stack like one block.
          .map((line) => (mirror ? line.padStart(cols, " ").slice(cols - keep) : line.slice(0, keep).replace(/\s+$/, "") || " "))
          .join("\n"),
      ),
    );
  }
  return out;
}

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

// ---- 5. "thinking" animations ------------------------------------------------------------------------------
// One text line each (the bunny takes two), eight cells wide, shown beside "Thinking…" while an agent works.
// The user picks one in Settings › Appearance. Only glyphs of the bundled mono face are used.
const TW = 8;
const thinkBars = Array.from({ length: 24 }, (_, f) => {
  const g = "▁▂▃▄▅▆▇█";
  let s = "";
  for (let i = 0; i < TW; i += 1) {
    // Two sines of different period, so the equalizer never looks mechanical.
    const v = 0.5 + 0.32 * Math.sin(((f + i * 2.6) / 12) * Math.PI * 2) + 0.18 * Math.sin(((f * 2 + i * 5.1) / 12) * Math.PI * 2);
    s += g[Math.max(0, Math.min(g.length - 1, Math.round(v * (g.length - 1))))];
  }
  return s;
});
const thinkPulse = (() => {
  const path = [];
  for (let i = 0; i < TW; i += 1) path.push(i);
  for (let i = TW - 2; i > 0; i -= 1) path.push(i);
  return path.map((head) => {
    let s = "";
    for (let i = 0; i < TW; i += 1) {
      const d = Math.abs(i - head);
      s += d === 0 ? "●" : d === 1 ? "•" : d === 2 ? "∙" : " ";
    }
    return s;
  });
})();
const thinkQuads = Array.from({ length: 4 }, (_, f) => {
  const g = "▖▘▝▗";
  let s = "";
  for (let i = 0; i < TW; i += 1) s += g[(f + i) % 4];
  return s;
});
const thinkCharge = (() => {
  const g = " ▏▎▍▌▋▊▉█";
  const out = [];
  // Fills in half-cell steps, then the block slides out to the right.
  for (let n = 0; n <= TW * 8; n += 4) {
    let s = "";
    for (let i = 0; i < TW; i += 1) s += g[Math.max(0, Math.min(8, n - i * 8))];
    out.push(s);
  }
  for (let n = 1; n <= TW; n += 1) out.push(" ".repeat(n) + "█".repeat(TW - n));
  return out;
})();
const thinkMoon = Array.from({ length: 6 }, (_, f) => {
  const g = "◌◔◕●◕◔";
  let s = "";
  for (let i = 0; i < TW; i += 1) s += g[(((f - i) % 6) + 6) % 6];
  return s;
});
const thinkCipher = (() => {
  // Quadrant glyphs flicker like a decode, lock in from the left, then start over.
  const g = "▚▞▙▟▛▜▖▗▘▝▌▐▀▄";
  let seed = 11;
  const rnd = () => ((seed = (seed * 1103515245 + 12345) & 0x7fffffff), seed / 0x7fffffff);
  const out = [];
  for (let f = 0; f < 20; f += 1) {
    const solid = Math.max(0, f - 8);
    let s = "";
    for (let i = 0; i < TW; i += 1) s += i < solid ? "█" : g[Math.floor(rnd() * g.length)];
    out.push(s);
  }
  return out;
})();
const thinkChevrons = (() => {
  const out = [];
  for (let f = 0; f < TW + 3; f += 1) {
    let s = "";
    for (let i = 0; i < TW; i += 1) s += i <= f && i > f - 3 ? "❯" : " ";
    out.push(s);
  }
  return out;
})();
const thinkBunny = (() => {
  // The mascot hops there and back (two lines: ears, face), blinking at each end.
  const span = 5;
  const out = [];
  const pose = (x, ears, face) => `${" ".repeat(x)}${ears}\n${" ".repeat(x)}${face}`;
  const xs = [];
  for (let i = 0; i <= span; i += 1) xs.push(i);
  for (let i = span - 1; i > 0; i -= 1) xs.push(i);
  xs.forEach((x, k) => {
    const right = k <= span;
    out.push(pose(x, right ? "(\\(\\" : " /)/)", right ? "( •.•)" : "(•.• )"));
    if (x === 0 || x === span) out.push(pose(x, right ? "(\\(\\" : " /)/)", right ? "( -.-)" : "(-.- )"));
  });
  return out;
})();
// `divider`: busy-clock ticks (90 ms) per frame.
const THINK = [
  { id: "scan", divider: 1, frames: scan },
  { id: "bars", divider: 1, frames: thinkBars },
  { id: "pulse", divider: 1, frames: thinkPulse },
  { id: "quads", divider: 2, frames: thinkQuads },
  { id: "charge", divider: 1, frames: thinkCharge },
  { id: "moon", divider: 2, frames: thinkMoon },
  { id: "cipher", divider: 1, frames: thinkCipher },
  { id: "chevrons", divider: 1, frames: thinkChevrons },
  { id: "bunny", divider: 3, frames: thinkBunny },
];
const thinkCells = (t) => Math.max(...t.frames.flatMap((f) => f.split("\n").map((l) => [...l].length)));
const thinkLines = (t) => Math.max(...t.frames.map((f) => f.split("\n").length));
const thinkIndex = `${THINK.map((t, i) => (i === 0 ? "" : `id == "${t.id}" ? ${i} : `)).join("")}0`;

const body = `// Generated by scripts/ui/gen-ascii-art.mjs. Do not edit by hand.
// Unicode / ASCII art frames: block wordmark, the ASCII bunny, the dither field and the glyph spinners.
export global AsciiArt {
    // "WORSE" over "HARNESS" in 5x7 block capitals; the solid letter walks through the wordmark.
    out property <[string]> wordmark: ${slintArray(wordmarkFrames(["WORSE", "HARNESS"], 48))};
    out property <[string]> wordmark-short: ${slintArray(wordmarkFrames(["HARNESS"], 28))};
    out property <[string]> bunny: ${slintArray(bunnyIdle)};
    out property <[string]> bunny-sleep: ${slintArray(bunnySleep)};
    out property <[string]> bunny-work: ${slintArray(bunnyWork)};
    // Per letter ([letter][frame]): W O R S E, then H A R N E S S.
    out property <[[string]]> wordmark-letters: ${slintArray2(wordmarkLetterFrames(["WORSE", "HARNESS"], 48))};
    // Dither field, [band][frame]: 6 bands of 18 rows; 32 visible columns of a 46-column field.
    out property <[[string]]> field-left: ${slintArray2(fieldBands(46, 32, 108, 6, 6, false, 3))};
    out property <[[string]]> field-right: ${slintArray2(fieldBands(46, 32, 108, 6, 6, true, 19))};
    out property <[string]> spinner: ${slintArray(spinner)};
    out property <[string]> scan: ${slintArray(scan)};
    out property <[string]> bars: ${slintArray(bars)};
    // "Thinking" animations, [style][frame]: ${THINK.map((t) => t.id).join(", ")}.
    out property <[[string]]> think: ${slintArray2(THINK.map((t) => t.frames))};
    out property <[string]> think-ids: [${THINK.map((t) => `"${t.id}"`).join(", ")}];
    // Busy-clock ticks per frame, widest line in cells, and text lines of each style.
    out property <[int]> think-divider: [${THINK.map((t) => t.divider).join(", ")}];
    out property <[int]> think-cells: [${THINK.map(thinkCells).join(", ")}];
    out property <[int]> think-lines: [${THINK.map(thinkLines).join(", ")}];
    pure public function think-index(id: string) -> int {
        return ${thinkIndex};
    }
}
`;
writeFileSync(OUT, body);
console.log(`wrote ${OUT} (${body.length} bytes)`);
