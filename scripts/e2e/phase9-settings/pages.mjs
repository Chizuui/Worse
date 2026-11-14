// Harness settings pages (id, English nav label) and the ZCode section each one is compared with.
export const PAGES = [
  ["general", "General"],
  ["appearance", "Appearance"],
  ["agents", "Agents"],
  ["shortcuts", "Keyboard Shortcuts"],
  ["instructions", "Instructions"],
  ["memory", "Memory"],
  ["subagent", "Subagents"],
  ["mcp", "MCP Servers"],
  ["skill", "Skills"],
  ["command", "Commands"],
  ["hook", "Hooks"],
  ["projects", "Projects"],
  ["archive", "Archive management"],
];

/** ZCode `data-settings-section-nav` ids. Agents ≈ ZCode "modelProvider", Instructions ≈ "personalization". */
export const ZCODE_SECTION = {
  general: "general",
  appearance: "appearance",
  agents: "modelProvider",
  shortcuts: "shortcuts",
  instructions: "personalization",
  memory: "memory",
  subagent: "subagents",
  mcp: "mcp",
  skill: "skill",
  command: "commands",
  hook: "hooks",
  projects: "projects",
  archive: "archives",
};
