export type BridgeLogLevel = "debug" | "info" | "warn" | "error";

/** Injected by host-node so the bridge shares its stderr log format and level filter. */
export type BridgeLogger = (level: BridgeLogLevel, message: string, details?: unknown) => void;
