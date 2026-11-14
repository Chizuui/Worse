export type LogLevel = "debug" | "info" | "warn" | "error";

const LEVEL_WEIGHT: Record<LogLevel, number> = {
  debug: 10,
  info: 20,
  warn: 30,
  error: 40
};

function parseLevel(raw: string | undefined): LogLevel {
  if (raw === "debug" || raw === "info" || raw === "warn" || raw === "error") return raw;
  return "info";
}

const threshold = LEVEL_WEIGHT[parseLevel(process.env.HARNESS_LOG_LEVEL)];

function formatDetails(details: unknown): string {
  if (details instanceof Error) return details.stack ?? `${details.name}: ${details.message}`;
  try {
    return JSON.stringify(details);
  } catch {
    return String(details);
  }
}

export function log(level: LogLevel, message: string, details?: unknown): void {
  if (LEVEL_WEIGHT[level] < threshold) return;
  const suffix = details === undefined ? "" : ` ${formatDetails(details)}`;
  process.stderr.write(`[harness-host-node] ${level} ${message}${suffix}\n`);
}
