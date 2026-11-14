/**
 * Splits a byte stream into `\n`-terminated UTF-8 lines.
 *
 * Chunks are kept as Buffers and only the new chunk is scanned for `\n`, so a
 * 30 MiB base64 preview line arriving in 64 KiB chunks costs linear time
 * (string concatenation + re-splitting would be quadratic).
 */
export interface LineSplitter {
  push(chunk: Buffer): void;
  /** Returns the unterminated remainder, if any (called at stream end). */
  flush(): string | null;
}

export function createLineSplitter(
  maxLineBytes: number,
  onLine: (line: string) => void,
  onOverflow: () => void
): LineSplitter {
  let pending: Buffer[] = [];
  let pendingBytes = 0;
  let overflowed = false;

  const emit = (parts: Buffer[]): void => {
    const text = Buffer.concat(parts).toString("utf8").trim();
    if (text.length > 0) onLine(text);
  };

  const push = (chunk: Buffer): void => {
    if (overflowed) return;
    let start = 0;
    for (let newline = chunk.indexOf(0x0a, start); newline !== -1; newline = chunk.indexOf(0x0a, start)) {
      const piece = chunk.subarray(start, newline);
      if (pendingBytes + piece.byteLength > maxLineBytes) {
        overflowed = true;
        onOverflow();
        return;
      }
      pending.push(piece);
      const parts = pending;
      pending = [];
      pendingBytes = 0;
      emit(parts);
      start = newline + 1;
    }
    if (start < chunk.byteLength) {
      const rest = chunk.subarray(start);
      pendingBytes += rest.byteLength;
      if (pendingBytes > maxLineBytes) {
        overflowed = true;
        pending = [];
        onOverflow();
        return;
      }
      pending.push(rest);
    }
  };

  const flush = (): string | null => {
    if (pending.length === 0) return null;
    const text = Buffer.concat(pending).toString("utf8").trim();
    pending = [];
    pendingBytes = 0;
    return text.length > 0 ? text : null;
  };

  return { push, flush };
}
