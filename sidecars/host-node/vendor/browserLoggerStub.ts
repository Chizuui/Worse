// Stand-in for packages/desktop/src/main/logger.ts inside the vendored browser owner. Main's logger
// writes rotating files under the ZCode config dir at import time; the sidecar logs to stderr instead
// (the Rust shell captures it), so the vendored executors keep their call sites unchanged.
type Writer = (...args: unknown[]) => void;

function write(level: string): Writer {
  return (...args: unknown[]) => {
    if (level === "debug" && process.env.HARNESS_LOG_LEVEL !== "debug") return;
    const text = args
      .map((arg) => (typeof arg === "string" ? arg : arg instanceof Error ? arg.message : safeJson(arg)))
      .join(" ");
    process.stderr.write(`[harness-host-node] ${level} [browser] ${text}\n`);
  };
}

function safeJson(value: unknown): string {
  try {
    return JSON.stringify(value);
  } catch {
    return String(value);
  }
}

export const logger = {
  debug: write("debug"),
  info: write("info"),
  warn: write("warn"),
  error: write("error"),
};

export default logger;
