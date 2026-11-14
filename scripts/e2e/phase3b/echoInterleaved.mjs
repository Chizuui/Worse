// Echo latency, TS vs Rust, measured side by side: two host-node sidecars (TS-only and Rust pty) are
// open at the same time and the echo reps alternate between them (ABBA order), so machine drift
// between the sequential TS and Rust runs of the main gate does not land in the ratio.
import { mkdir } from "node:fs/promises";
import { openHostSession } from "../phase3/hostSession.mjs";
import { echoRoundtrip, openTerminal, waitQuiet } from "./terminal.mjs";
import { createTreeTracker } from "./processes.mjs";
import { ScenarioError } from "./ptyScenarios.mjs";

const WARMUP_REPS = 3;
const QUIET_MS = 60;

export async function runInterleavedEcho(modules, { cwd, reps, tsEnv, rustEnv, expectRust = true }) {
  await mkdir(cwd, { recursive: true });
  const sides = [
    { label: "ts", env: tsEnv },
    { label: "rust", env: rustEnv },
  ];
  const opened = [];
  try {
    for (const side of sides) {
      const session = await openHostSession({ modules, mode: `echo-${side.label}`, env: side.env, step: `P3B_ECHO_${side.label.toUpperCase()}` });
      const tracker = createTreeTracker(session.sidecar.child.pid);
      opened.push({ ...side, session, tracker, samples: [], firstData: [] });
    }
    for (const side of opened) {
      side.term = await openTerminal(side.session.client, { cols: 80, rows: 24, cwd });
      if (!(await waitQuiet(side.term))) throw new ScenarioError("ECHO", `[${side.label}] shell produced no prompt`);
      side.tracker.snapshot();
    }
    const [ts, rust] = opened;
    if (String(ts.term.id).startsWith("rpty-")) throw new ScenarioError("ECHO", "TS side was served by the bridge");
    if (expectRust && !String(rust.term.id).startsWith("rpty-")) throw new ScenarioError("ECHO", `Rust side ${rust.term.id} was not served by Rust`);
    for (let rep = 0; rep < WARMUP_REPS + reps; rep += 1) {
      // ABBA：每两轮交换先后顺序，抵消"谁先跑"带来的系统性偏差。
      const order = rep % 2 === 0 ? [ts, rust] : [rust, ts];
      for (const side of order) {
        const result = await echoRoundtrip(side.term, "IL", `${100 + rep}`);
        if (!result.seen) throw new ScenarioError("ECHO", `[${side.label}] interleaved marker ${rep} not seen`);
        if (rep >= WARMUP_REPS) {
          side.samples.push(result.ms);
          if (result.firstDataMs !== null) side.firstData.push(result.firstDataMs);
        }
        // 等这个 shell 画完提示符再测另一个：两个 pwsh 的收尾输出互相重叠会把抖动带进计时（不计入耗时）。
        await waitQuiet(side.term, QUIET_MS);
      }
    }
    for (const side of opened) {
      side.term.unsubscribe();
      await side.term.dispose().catch(() => undefined);
    }
    return { ts: ts.samples, rust: rust.samples, firstData: { ts: ts.firstData, rust: rust.firstData }, trackers: { ts: ts.tracker, rust: rust.tracker } };
  } finally {
    for (const side of opened) await side.session.close();
  }
}
