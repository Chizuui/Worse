export interface LineReaderHandlers {
  maxLineBytes: number;
  onLine(line: string): void;
  onOverflow(): void;
  onEnd(): void;
  onError(error: Error): void;
}

/**
 * Splits a UTF-8 stream into `\n`-terminated lines and enforces the line limit.
 *
 * The pending remainder is checked by character count (a cheap lower bound of
 * its byte size); every complete line is checked by exact byte length.
 */
export function readLines(stream: NodeJS.ReadableStream, handlers: LineReaderHandlers): void {
  let remainder = "";
  let overflowed = false;

  const overflow = (): void => {
    if (overflowed) return;
    overflowed = true;
    remainder = "";
    handlers.onOverflow();
  };

  stream.setEncoding("utf8");
  stream.on("data", (chunk: string | Buffer) => {
    if (overflowed) return;
    const parts = (remainder + chunk.toString()).split("\n");
    remainder = parts.pop() ?? "";
    for (const part of parts) {
      if (Buffer.byteLength(part, "utf8") > handlers.maxLineBytes) {
        overflow();
        return;
      }
      const line = part.trim();
      if (line.length > 0) handlers.onLine(line);
    }
    if (remainder.length > handlers.maxLineBytes) overflow();
  });
  stream.on("end", () => handlers.onEnd());
  stream.on("error", (error: Error) => handlers.onError(error));
}
