#!/usr/bin/env node
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const worse = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const repo = path.resolve(worse, "..");
const materialDir = path.join(repo, "packages", "desktop", "src", "renderer", "public", "material-icons");
const uiDir = path.join(worse, "apps", "harness-slint", "ui");
const outDir = path.join(uiDir, "icons");

const LINE = {
  plus: ["plus-sign", "add-01"],
  "settings-2": ["settings-01", "setting-07"],
  palette: ["paint-board", "colors"],
  keyboard: ["keyboard"],
  user: ["user-circle", "user"],
  brain: ["ai-brain-01", "brain"],
  cable: ["plug-socket", "plug-01"],
  "wand-sparkles": ["magic-wand-01", "sparkles"],
  anchor: ["webhook", "anchor"],
  folders: ["folder-library", "folder-02"],
  archive: ["archive-02", "archive"],
  pencil: ["pencil-edit-02", "edit-02"],
  "rotate-ccw": ["arrow-turn-backward", "undo-02"],
  "file-code": ["source-code", "file-code"],
  minus: ["minus-sign"],
  scan: ["center-focus", "scan"],
  network: ["hierarchy-square-02", "hierarchy", "connect"],
  "arrow-left": ["arrow-left-02"],
  code: ["source-code", "code"],
  "square-pen": ["pencil-edit-02", "edit-02"],
  settings: ["settings-02", "settings-01"],
  search: ["search-01"],
  "chevron-down": ["arrow-down-01"],
  "chevron-right": ["arrow-right-01"],
  "chevron-up": ["arrow-up-01"],
  "chevron-left": ["arrow-left-01"],
  check: ["tick-02", "checkmark"],
  x: ["cancel-01"],
  "arrow-up": ["arrow-up-02"],
  "arrow-down": ["arrow-down-02"],
  square: ["stop", "square"],
  folder: ["folder-01"],
  "folder-open": ["folder-open"],
  file: ["file-01", "file-02"],
  "file-text": ["note-01", "file-02"],
  "file-diff": ["git-compare", "file-diff"],
  terminal: ["command-line", "computer-terminal-01"],
  "square-terminal": ["computer-terminal-01", "command-line"],
  "git-branch": ["git-branch"],
  "panel-left": ["sidebar-left-01", "sidebar-left"],
  "panel-right": ["sidebar-right-01", "sidebar-right"],
  "message-square": ["bubble-chat", "message-01"],
  copy: ["copy-01"],
  "loader-circle": ["loading-03", "loading-02"],
  "circle-check": ["checkmark-circle-02", "checkmark-circle-01"],
  "circle-x": ["cancel-circle"],
  "circle-alert": ["alert-circle"],
  "triangle-alert": ["alert-02", "alert-01"],
  "shield-check": ["security-check", "shield-01"],
  pause: ["pause"],
  wrench: ["wrench-01"],
  bot: ["robotic", "robot-01", "bot"],
  sun: ["sun-03", "sun-01"],
  moon: ["moon-02", "moon-01"],
  monitor: ["computer", "laptop"],
  languages: ["translate", "language-skill"],
  info: ["information-circle"],
  ellipsis: ["more-horizontal"],
  "trash-2": ["delete-02", "delete-01"],
  clock: ["clock-01"],
  "list-ordered": ["left-to-right-list-number", "list-view"],
  "arrow-left-right": ["arrow-data-transfer-horizontal", "arrow-left-right"],
  "refresh-cw": ["refresh", "reload"],
  "external-link": ["link-square-02", "arrow-up-right-01"],
  "at-sign": ["at"],
  paperclip: ["attachment-01", "attachment"],
  "corner-down-left": ["arrow-turn-backward", "enter"],
  brush: ["paint-brush-01", "brush"],
  "calendar-clock": ["calendar-03", "calendar-clock"],
  "git-fork": ["git-fork"],
  "grip-vertical": ["drag-drop-vertical"],
  image: ["image-01"],
  "list-todo": ["task-01", "check-list"],
  mail: ["mail-01"],
  pin: ["pin"],
  play: ["play"],
  "rotate-cw": ["arrow-turn-forward", "redo-02"],
  table: ["table-01"],
  "sliders-horizontal": ["filter-horizontal", "slider-horizontal"],
  "maximize-2": ["arrow-expand-01", "maximize-01"],
  "minimize-2": ["arrow-shrink-01", "minimize-01"],
  sparkles: ["sparkles", "ai-magic"],
  home: ["home-01"],
  history: ["clock-04", "time-quarter-pass"],
  globe: ["globe-02", "global"],
  eye: ["view"],
  download: ["download-01"],
  upload: ["upload-01"],
  link: ["link-01"],
  filter: ["filter"],
  layers: ["layers-01"],
  star: ["star"],
  "arrow-right": ["arrow-right-02"],
  command: ["command"],
  cursor: ["cursor-01"],
};

const FILL = {
  dot: ["circle-fill"],
  "circle-check": ["check-circle-fill"],
  "circle-x": ["x-circle-fill"],
  "circle-alert": ["warning-circle-fill"],
  "triangle-alert": ["warning-fill"],
  info: ["info-fill"],
  play: ["play-fill"],
  pause: ["pause-fill"],
  stop: ["stop-fill"],
  send: ["arrow-up-bold"],
  pin: ["push-pin-fill"],
  star: ["star-fill"],
  sparkle: ["sparkle-fill"],
  lightning: ["lightning-fill"],
  "shield-check": ["shield-check-fill"],
  folder: ["folder-simple-fill", "folder-fill"],
  chat: ["chat-circle-dots-fill"],
  "caret-down": ["caret-down-fill"],
  "caret-right": ["caret-right-fill"],
  check: ["check-bold"],
  x: ["x-bold"],
};

const TILE = {
  terminal: ["code-square-bold-duotone", "command-bold-duotone"],
  edit: ["pen-new-square-bold-duotone", "pen-bold-duotone"],
  read: ["document-text-bold-duotone", "file-text-bold-duotone"],
  search: ["magnifer-bold-duotone"],
  folder: ["folder-with-files-bold-duotone", "folder-bold-duotone"],
  globe: ["global-bold-duotone"],
  shield: ["shield-check-bold-duotone"],
  cpu: ["cpu-bolt-bold-duotone", "cpu-bold-duotone"],
  settings: ["settings-bold-duotone"],
  general: ["tuning-2-bold-duotone", "settings-minimalistic-bold-duotone"],
  palette: ["palette-round-bold-duotone", "palette-bold-duotone"],
  keyboard: ["keyboard-bold-duotone"],
  user: ["user-circle-bold-duotone"],
  brain: ["lightbulb-bolt-bold-duotone", "lightbulb-bold-duotone"],
  plug: ["plug-circle-bold-duotone"],
  magic: ["magic-stick-3-bold-duotone", "magic-stick-bold-duotone"],
  hook: ["link-round-angle-bold-duotone", "link-bold-duotone"],
  archive: ["archive-bold-duotone"],
  history: ["history-bold-duotone"],
  chat: ["chat-round-dots-bold-duotone"],
  branch: ["branching-paths-up-bold-duotone", "routing-2-bold-duotone"],
  image: ["gallery-bold-duotone"],
  calendar: ["calendar-mark-bold-duotone"],
  checklist: ["checklist-minimalistic-bold-duotone"],
  monitor: ["monitor-bold-duotone"],
  bug: ["bug-bold-duotone"],
  database: ["database-bold-duotone"],
  danger: ["danger-triangle-bold-duotone"],
  info: ["info-circle-bold-duotone"],
  check: ["check-circle-bold-duotone"],
  close: ["close-circle-bold-duotone"],
  stars: ["stars-bold-duotone"],
  bolt: ["bolt-bold-duotone"],
  document: ["document-bold-duotone"],
  cursor: ["cursor-bold-duotone"],
  widget: ["widget-5-bold-duotone", "widget-bold-duotone"],
  network: ["structure-bold-duotone", "server-square-bold-duotone"],
  robot: ["ghost-smile-bold-duotone", "smile-circle-bold-duotone"],
  clipboard: ["clipboard-list-bold-duotone", "clipboard-text-bold-duotone"],
  compass: ["compass-big-bold-duotone", "compass-bold-duotone"],
};

const MATERIAL = [
  "folder", "document", "rust", "markdown", "readme", "typescript", "react_ts", "react", "javascript", "json",
  "toml", "yaml", "python", "go", "html", "css", "git", "lock", "image", "console", "settings", "svg", "docker",
  "database", "java", "nodejs_alt", "npm", "tsconfig",
];

function loadSet(src, name) {
  const file = path.join(src, "@iconify-json", name, "icons.json");
  if (!existsSync(file)) throw new Error(`missing ${file}; pass the node_modules dir that holds @iconify-json/{hugeicons,ph,solar}`);
  return JSON.parse(readFileSync(file, "utf8"));
}

function resolve(set, name) {
  const icon = set.icons[name];
  if (icon) return { body: icon.body, width: icon.width ?? set.width ?? 16, height: icon.height ?? set.height ?? 16, left: icon.left ?? 0, top: icon.top ?? 0 };
  const alias = set.aliases?.[name];
  if (!alias || alias.rotate || alias.hFlip || alias.vFlip) return null;
  const parent = resolve(set, alias.parent);
  if (!parent) return null;
  return { ...parent, width: alias.width ?? parent.width, height: alias.height ?? parent.height };
}

function svgOf(icon) {
  const body = icon.body.replaceAll("currentColor", "#000");
  return `<svg xmlns="http://www.w3.org/2000/svg" width="${icon.width}" height="${icon.height}" viewBox="${icon.left} ${icon.top} ${icon.width} ${icon.height}">${body}</svg>\n`;
}

function emit(set, table, dir, missing) {
  mkdirSync(path.join(outDir, dir), { recursive: true });
  const picked = {};
  for (const [key, candidates] of Object.entries(table)) {
    const name = candidates.find((candidate) => resolve(set, candidate));
    if (!name) {
      missing.push(`${dir}:${key} (${candidates.join(", ")})`);
      continue;
    }
    writeFileSync(path.join(outDir, dir, `${key}.svg`), svgOf(resolve(set, name)));
    picked[key] = name;
  }
  return picked;
}

function main() {
  const src = process.argv[2];
  if (!src) throw new Error("usage: node gen-slint-icons.mjs <node_modules dir with @iconify-json/{hugeicons,ph,solar}>");
  const huge = loadSet(src, "hugeicons");
  const ph = loadSet(src, "ph");
  const solar = loadSet(src, "solar");
  rmSync(outDir, { recursive: true, force: true });
  mkdirSync(path.join(outDir, "files"), { recursive: true });
  const missing = [];
  const line = emit(huge, LINE, "line", missing);
  const fill = emit(ph, FILL, "fill", missing);
  const tile = emit(solar, TILE, "tile", missing);
  if (missing.length) throw new Error(`unresolved icons:\n${missing.join("\n")}`);
  for (const name of MATERIAL) {
    const svg = readFileSync(path.join(materialDir, `${name}.svg`), "utf8").replace(/ style="[^"]*display-p3[^"]*"/g, "");
    writeFileSync(path.join(outDir, "files", `${name}.svg`), svg);
  }

  const prop = (key) => key.replaceAll("_", "-");
  const block = (global, table, dir) => [
    `export global ${global} {`,
    ...Object.keys(table).map((key) => `    out property <image> ${prop(key)}: @image-url("../icons/${dir}/${key}.svg");`),
    "}",
    "",
  ];
  const slint = [
    "// GENERATED by Worse/scripts/ui/gen-slint-icons.mjs. Licenses: ui/icons/LICENSES.md.",
    "",
    ...block("Icons", line, "line"),
    ...block("IconsFill", fill, "fill"),
    ...block("Tiles", tile, "tile"),
    "export global FileIcons {",
    `    out property <[image]> all: [${MATERIAL.map((name) => `@image-url("../icons/files/${name}.svg")`).join(", ")}];`,
    "}",
    "",
  ].join("\n");
  writeFileSync(path.join(uiDir, "components", "icons.slint"), slint);
  writeFileSync(path.join(outDir, "files", "INDEX.txt"), `${MATERIAL.join("\n")}\n`);
  writeFileSync(
    path.join(outDir, "SOURCES.txt"),
    [
      ...Object.entries(line).map(([key, name]) => `line/${key}.svg <- hugeicons:${name}`),
      ...Object.entries(fill).map(([key, name]) => `fill/${key}.svg <- ph:${name}`),
      ...Object.entries(tile).map(([key, name]) => `tile/${key}.svg <- solar:${name}`),
      "",
    ].join("\n"),
  );

  const info = (set) => `${set.info?.name ?? set.prefix} ${set.info?.version ?? ""}`.trim();
  writeFileSync(
    path.join(outDir, "LICENSES.md"),
    [
      "# Icon sources and licenses",
      "",
      `- \`line/\`: ${info(huge)} (Hugeicons free stroke-rounded set, https://hugeicons.com), MIT License. Generated from \`@iconify-json/hugeicons\`.`,
      `- \`fill/\`: ${info(ph)} (https://phosphoricons.com), MIT License, Copyright (c) 2023 Phosphor Icons. Generated from \`@iconify-json/ph\`.`,
      `- \`tile/\`: ${info(solar)} by 480 Design (https://www.figma.com/community/file/1166831539721848736), Creative Commons Attribution 4.0`,
      "  International (CC BY 4.0, https://creativecommons.org/licenses/by/4.0/). Generated from `@iconify-json/solar`; colors replaced",
      "  with black so the app can tint them.",
      "- `files/`: copied from ZCode `packages/desktop/src/renderer/public/material-icons` (Material Icon Theme by Philipp Kief,",
      "  MIT License, https://github.com/material-extensions/vscode-material-icon-theme).",
      "",
      "SOURCES.txt lists the upstream icon name behind every generated file.",
      "",
      "## MIT License (Hugeicons, Phosphor, Material Icon Theme)",
      "",
      "```",
      "Permission is hereby granted, free of charge, to any person obtaining a copy of this software and associated",
      "documentation files (the \"Software\"), to deal in the Software without restriction, including without limitation the",
      "rights to use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies of the Software, and to permit",
      "persons to whom the Software is furnished to do so, subject to the following conditions:",
      "",
      "The above copyright notice and this permission notice shall be included in all copies or substantial portions of the",
      "Software.",
      "",
      "THE SOFTWARE IS PROVIDED \"AS IS\", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE",
      "WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR",
      "COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR",
      "OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.",
      "```",
      "",
    ].join("\n"),
  );
  process.stdout.write(`icons: ${Object.keys(line).length} line, ${Object.keys(fill).length} fill, ${Object.keys(tile).length} tile, ${MATERIAL.length} files\n`);
}

main();
