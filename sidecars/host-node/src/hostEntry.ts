import { stat } from "node:fs/promises";
import { pathToFileURL } from "node:url";

export async function importHostEntry(entryPath: string): Promise<void> {
  const info = await stat(entryPath).catch(() => null);
  if (!info || !info.isFile()) {
    throw new Error(`Host entry is not a readable file: ${entryPath}`);
  }
  await import(pathToFileURL(entryPath).href);
}
