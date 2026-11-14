/** Same cap as `TERMINAL_UNSUBSCRIBED_OUTPUT_MAX_CHARS` in ZCode `terminalService.ts`. */
export const UNSUBSCRIBED_OUTPUT_MAX_CHARS = 1_048_576;

/**
 * Output kept while a terminal has no data listener, replayed in order to the
 * next first listener. Mirrors `createBufferedTerminalOutputEmitter`: bounded by
 * UTF-16 length, oldest chunks dropped first, the newest chunk always kept.
 */
export interface OutputBuffer {
  push(data: string): void;
  /** Returns and clears the buffered chunks. */
  drain(): string[];
}

export function createOutputBuffer(maxChars = UNSUBSCRIBED_OUTPUT_MAX_CHARS): OutputBuffer {
  let pending: string[] = [];
  let pendingChars = 0;
  return {
    push(data) {
      pending.push(data);
      pendingChars += data.length;
      while (pendingChars > maxChars && pending.length > 1) {
        pendingChars -= pending.shift()?.length ?? 0;
      }
    },
    drain() {
      const chunks = pending;
      pending = [];
      pendingChars = 0;
      return chunks;
    }
  };
}
