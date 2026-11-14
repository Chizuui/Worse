// Bundle entry: ZCode's terminal policy functions, re-exported verbatim from the ZCode source
// (read-only; never edited from Harness). Built by `scripts/build-vendor.mjs` into
// `dist-vendor/terminalPolicy.js` and loaded by `src/sessions/terminalPolicy.ts`.
export { resolveTerminalFontProfile } from "../../../../packages/services/src/terminal/terminalProfile.ts";
export {
  resolveTerminalCwd,
  resolveTerminalEnv,
  resolveTerminalShell,
  resolveTerminalWindowsPtyInfo
} from "../../../../packages/services/src/terminal/terminalService.ts";
