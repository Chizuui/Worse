// Comparison scenes for the Harness UI (spec phase-8-ui.md §8 "ZCode comparison"), driven through the Slint MCP
// server against the built-in fake core (HARNESS_UI_CORE=fake). They mirror zcode-reference.mjs: the same window
// size, the same "plan" markdown (mock provider P7_MD_* = fake core PLAN_REPLY), a permission prompt and settings.
// Used by the phase8-ui gate (which also asserts the markdown checks) and by compare.mjs.
import { byLabel, label, sleep } from "./mcp.mjs";

export const SCENES = ["light-empty", "light-chat", "dark-chat", "dark-permission", "light-permission", "light-settings", "dark-settings"];

const isButton = (text) => (e) => label(e) === text && e.accessibleRole === "Button";

/**
 * Runs the scenes, calling `save(name)` for each screenshot. Returns the markdown checks:
 * `{ table, code, codeHighlighted }` (accessible labels/descriptions of the rendered blocks).
 */
export async function captureScenes(mcp, save) {
  const press = async (text) => mcp.activate(await mcp.waitFor(`button "${text}"`, isButton(text)));
  // Phase 9.1: the theme control lives on the Settings "Appearance" page.
  const theme = async (name) => {
    await press("Settings");
    await press("Appearance");
    await press(name);
    await mcp.key("\u001b");
    await mcp.waitGone("settings page", byLabel("Close settings"));
    await sleep(400);
  };
  const send = async (text) => {
    const input = await mcp.waitFor("message input", byLabel("Message"));
    await mcp.click(input);
    await mcp.setValue(input, text);
    await mcp.key("\n");
  };

  await theme("Light");
  await save("light-empty");

  await send("show me the plan");
  await mcp.waitFor("plan reply", (e) => label(e).includes(": ") && label(e).includes("Report the result"), 30_000);
  await mcp.waitGone("streaming", byLabel("Streaming…"), 30_000);
  const table = await mcp.waitFor("rendered table", (e) => label(e).startsWith("table "));
  const code = await mcp.waitFor("rendered code block", (e) => label(e) === "code rust");
  await sleep(400);
  await save("light-chat");
  await theme("Dark");
  await save("dark-chat");

  await send("write the file, permission please");
  // 9.2 起卡片列出 agent 的选项（fake core: Allow / Always allow / Deny / Always deny），与 ZCode 的选项列表一致。
  await mcp.waitFor("permission prompt", isButton("Allow"), 30_000);
  await sleep(300);
  await save("dark-permission");
  await theme("Light");
  await save("light-permission");
  await press("Allow");
  await mcp.waitGone("permission prompt", isButton("Allow"));

  // ZCode's reference shots show its first settings page (General), so both themes are captured there.
  await press("Settings");
  await mcp.waitFor("settings page", byLabel("Close settings"));
  await press("General");
  await sleep(400);
  await save("light-settings");
  await press("Appearance");
  await press("Dark");
  await press("General");
  await sleep(400);
  await save("dark-settings");
  await press("Appearance");
  await press("Light");
  await mcp.key("\u001b");
  await mcp.waitGone("settings page", byLabel("Close settings"));
  return { table: label(table), code: label(code), codeHighlighted: code.accessibleDescription === "highlighted" };
}
