// File-watcher scenarios A1-A6 over the renderer RPC surface. Every mode gets its own fixture
// directory; paths are normalised to `<root>` so TS and Rust observations compare directly.
import { appendFile, mkdir, rename, rm, writeFile } from "node:fs/promises";
import path from "node:path";

const QUIET_MS = 700;
const MAX_WAIT_MS = 4_000;

function normaliser(root) {
  const variants = [root, root.replace(/\\/g, "/")];
  return (value) => {
    let text = JSON.stringify(value);
    for (const variant of variants) text = text.split(JSON.stringify(variant).slice(1, -1)).join("<root>");
    return JSON.parse(text);
  };
}

/** Subscribes to a watcher id and records events with arrival times. */
function listen(channel, id) {
  const events = [];
  const subscription = channel.listen("onDynamicChange", id)((event) => events.push({ at: performance.now(), event }));
  return { events, dispose: () => subscription.dispose() };
}

/** Runs `action`, then collects the events it caused until QUIET_MS without new events. */
async function step(recorder, action) {
  const from = recorder.events.length;
  const startedAt = performance.now();
  await action();
  const deadline = startedAt + MAX_WAIT_MS;
  let lastCount = from;
  let lastChange = performance.now();
  while (performance.now() < deadline) {
    await new Promise((resolve) => setTimeout(resolve, 25));
    if (recorder.events.length !== lastCount) {
      lastCount = recorder.events.length;
      lastChange = performance.now();
    } else if (performance.now() - lastChange >= QUIET_MS && (lastCount > from || performance.now() - startedAt >= QUIET_MS * 2)) {
      break;
    }
  }
  const batch = recorder.events.slice(from);
  return { events: batch.map((entry) => entry.event), latencyMs: batch.length > 0 ? batch[0].at - startedAt : null };
}

/** Distinct events, sorted: OS event splitting may differ run to run, the reported set may not. */
function eventSet(events) {
  return [...new Set(events.map((event) => JSON.stringify(event)))].sort();
}

async function watch(channel, params) {
  try {
    return { ok: true, id: (await channel.call("watch", [params])).id };
  } catch (error) {
    return { ok: false, message: error instanceof Error ? error.message : String(error) };
  }
}

export async function runWatchScenarios(client, root, mode) {
  const norm = normaliser(root);
  const channel = client.getChannel("file-watcher");
  const obs = {};
  const latency = [];
  const ids = [];
  await rm(root, { recursive: true, force: true });
  const dir = path.join(root, "a");
  await mkdir(dir, { recursive: true });

  // A1: create / modify / rename / delete a file.
  const w1 = await watch(channel, { path: dir });
  if (!w1.ok) throw new Error(`[${mode}] watch failed: ${w1.message}`);
  ids.push(w1.id);
  const r1 = listen(channel, w1.id);
  await new Promise((resolve) => setTimeout(resolve, 200));
  const file = path.join(dir, "f.txt");
  const steps = {
    create: () => writeFile(file, "1"),
    modify: () => appendFile(file, "2"),
    rename: () => rename(file, path.join(dir, "g.txt")),
    delete: () => rm(path.join(dir, "g.txt")),
  };
  obs.A1 = {};
  for (const [name, action] of Object.entries(steps)) {
    const result = await step(r1, action);
    obs.A1[name] = eventSet(norm(result.events));
    if (result.latencyMs !== null) latency.push(result.latencyMs);
  }

  // A2: burst of 20 writes in ~100 ms is coalesced.
  const burst = await step(r1, async () => {
    for (let index = 0; index < 20; index += 1) {
      await writeFile(path.join(dir, `burst-${index}.txt`), "x");
      await new Promise((resolve) => setTimeout(resolve, 5));
    }
  });
  obs.A2 = { coalesced: burst.events.length <= 2, set: eventSet(norm(burst.events)) };

  // A4: unwatch, then change -> silence.
  r1.dispose();
  await channel.call("unwatch", [{ id: w1.id }]);
  const after = listen(channel, w1.id);
  const silent = await step(after, () => writeFile(path.join(dir, "late.txt"), "x"));
  after.dispose();
  obs.A4 = { events: silent.events.length };

  // A3: recursive watch reports node_modules and .git changes (no ignore rules).
  const w3 = await watch(channel, { path: root, recursive: true });
  if (!w3.ok) throw new Error(`[${mode}] recursive watch failed: ${w3.message}`);
  ids.push(w3.id);
  const r3 = listen(channel, w3.id);
  await mkdir(path.join(root, "node_modules/x"), { recursive: true });
  await mkdir(path.join(root, ".git"), { recursive: true });
  await new Promise((resolve) => setTimeout(resolve, 300));
  const nm = await step(r3, () => writeFile(path.join(root, "node_modules/x/f.js"), "x"));
  const git = await step(r3, () => writeFile(path.join(root, ".git/y"), "x"));
  obs.A3 = { nodeModules: nm.events.length > 0, git: git.events.length > 0 };
  r3.dispose();
  await channel.call("unwatch", [{ id: w3.id }]);

  // A5: missing path -> same error text.
  const missing = await watch(channel, { path: path.join(root, "missing") });
  obs.A5 = norm(missing.ok ? { ok: true } : { ok: false, message: missing.message });

  // A6: deleting the watched directory -> final {dirPath}, then silence.
  const doomed = path.join(root, "doomed");
  await mkdir(doomed, { recursive: true });
  await writeFile(path.join(doomed, "keep.txt"), "x");
  const w6 = await watch(channel, { path: doomed });
  if (!w6.ok) throw new Error(`[${mode}] watch doomed failed: ${w6.message}`);
  ids.push(w6.id);
  const r6 = listen(channel, w6.id);
  await new Promise((resolve) => setTimeout(resolve, 200));
  const gone = await step(r6, () => rm(doomed, { recursive: true, force: true }));
  const final = gone.events.at(-1);
  await mkdir(doomed, { recursive: true });
  const reborn = await step(r6, () => writeFile(path.join(doomed, "new.txt"), "x"));
  r6.dispose();
  await channel.call("unwatch", [{ id: w6.id }]).catch(() => undefined);
  obs.A6 = { finalIsDirOnly: final !== undefined && norm(final).changedPath === undefined, eventsAfterRecreate: reborn.events.length };

  await rm(root, { recursive: true, force: true });
  return { obs, latency, ids };
}
