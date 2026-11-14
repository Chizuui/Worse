// PTY scenarios B1, B3-B8 over the renderer RPC surface. Each returns a comparable observation
// (ids normalised) plus latency samples; the gate compares TS vs Rust observations.
import { buildAttachServicePortMessage } from "../phase2/scenarios.mjs";
import { echoRoundtrip, openTerminal, waitQuiet, waitUntil } from "./terminal.mjs";

const ECHO_REPS = Number(process.env.PHASE3B_ECHO_REPS ?? 20);
const UNICODE = "中文 ✓ é";

export class ScenarioError extends Error {
  constructor(op, reason) {
    super(reason);
    this.op = op;
  }
}

function expect(op, condition, reason) {
  if (!condition) throw new ScenarioError(op, reason);
}

function normaliseCreate(created) {
  const { id: _id, ...rest } = created;
  return { keys: Object.keys(created), ...rest };
}

async function closeTerm(term) {
  term.unsubscribe();
  await term.dispose().catch(() => undefined);
}

export async function runPtyScenarios(session, { cwd, mode, onLive = () => undefined }) {
  const client = session.client;
  const obs = {};
  const perf = { spawnMs: [], echoMs: [], echoAckMs: [], echoFirstDataMs: [] };
  const params = { cols: 80, rows: 24, cwd };

  // B1: default shell, create shape, echo roundtrip.
  const t1 = await openTerminal(client, params);
  perf.spawnMs.push(t1.spawnMs);
  expect("B1", await waitQuiet(t1), `[${mode}] shell produced no prompt`);
  obs.B1 = { create: normaliseCreate(t1.created) };
  for (let rep = 0; rep < ECHO_REPS; rep += 1) {
    const result = await echoRoundtrip(t1, "MK", `${100 + rep}`);
    expect("B1", result.seen, `[${mode}] echo marker ${rep} not seen`);
    perf.echoMs.push(result.ms);
    perf.echoAckMs.push(result.writeAckMs);
    if (result.firstDataMs !== null) perf.echoFirstDataMs.push(result.firstDataMs);
  }
  obs.B1.echo = true;

  // B3: unicode output, built by the shell so the typed text does not match.
  const [left, right] = [UNICODE.slice(0, 2), UNICODE.slice(2)];
  const uni = await echoRoundtrip(t1, left, right);
  obs.B3 = { seen: uni.seen, replacementChar: t1.raw.includes("�") };

  // B4: resize, then ask the shell for its width.
  await t1.resize(100, 30);
  if (t1.commands.width) {
    await t1.run(t1.commands.width());
    obs.B4 = { width100: await waitUntil(() => t1.text().includes("W=100"), 10_000) };
  } else {
    obs.B4 = { width100: "n/a" };
  }

  // B6: shell exit code.
  await t1.run("exit 7");
  expect("B6", await waitUntil(() => t1.exitCode !== undefined), `[${mode}] no exit event`);
  obs.B6 = { exitCode: t1.exitCode };
  const afterExit = await t1.write("x").then(() => "ok", (error) => error.message.replace(t1.id, "<id>"));
  obs.B6.writeAfterExit = afterExit;
  t1.unsubscribe();

  // B5: dispose -> no exit event, later write rejected.
  const t5 = await openTerminal(client, params);
  perf.spawnMs.push(t5.spawnMs);
  await waitQuiet(t5);
  await t5.dispose();
  await new Promise((resolve) => setTimeout(resolve, 500));
  const afterDispose = await t5.write("x").then(() => "ok", (error) => error.message.replace(t5.id, "<id>"));
  obs.B5 = { exitEvent: t5.exitCode !== undefined, writeAfterDispose: afterDispose };
  t5.unsubscribe();

  // B7: two concurrent sessions, outputs do not cross.
  const [ta, tb] = await Promise.all([openTerminal(client, params), openTerminal(client, params)]);
  perf.spawnMs.push(ta.spawnMs, tb.spawnMs);
  await Promise.all([waitQuiet(ta), waitQuiet(tb)]);
  // 两个终端都存活时记录进程树，供关闭后的孤儿检查使用（不在计时区间内）。
  onLive();
  const [ra, rb] = await Promise.all([echoRoundtrip(ta, "AA", "111"), echoRoundtrip(tb, "BB", "222")]);
  obs.B7 = { aSeen: ra.seen, bSeen: rb.seen, crossed: ta.text().includes("BB222") || tb.text().includes("AA111") };
  await Promise.all([closeTerm(ta), closeTerm(tb)]);

  // B8: terminal created on a second desktop attachment survives that attachment's close; the
  // primary attachment re-subscribes and gets the output produced meanwhile (buffer replay).
  const extra = await session.attach(buildAttachServicePortMessage(), `P3B_${mode.toUpperCase()}_B8`);
  const t8 = await openTerminal(extra.client, params);
  await waitQuiet(t8);
  t8.unsubscribe();
  extra.close();
  await new Promise((resolve) => setTimeout(resolve, 300));
  const terminal = client.getChannel("terminal");
  const writeAfterClose = await terminal
    .call("write", [{ id: t8.id, data: `${t8.commands.print("RE", "PLAY")}${t8.commands.eol}` }])
    .then(() => "ok", (error) => error.message.replace(t8.id, "<id>"));
  await new Promise((resolve) => setTimeout(resolve, 1500));
  let replay = "";
  const sub = terminal.listen("onDynamicData", t8.id)((data) => {
    replay += data;
  });
  await waitUntil(() => replay.includes("REPLAY"), 5_000);
  sub.dispose();
  obs.B8 = { writeAfterClose, replayed: replay.includes("REPLAY") };
  await terminal.call("dispose", [{ id: t8.id }]).catch(() => undefined);

  return { obs, perf };
}

/** B9 (Rust mode only): harness-host dies under a live terminal. */
export async function runHostLossScenario(session, { cwd, killRustHost }) {
  const client = session.client;
  const term = await openTerminal(client, { cols: 80, rows: 24, cwd });
  await waitQuiet(term);
  expect("B9", String(term.id).startsWith("rpty-"), `terminal ${term.id} was not served by Rust`);
  killRustHost();
  const ended = await waitUntil(() => term.exitCode !== undefined, 10_000);
  expect("B9", ended, "no onDynamicExit after harness-host was killed (silent hang)");
  expect("B9", term.exitCode === -1, `exit code ${term.exitCode}, expected -1`);
  term.unsubscribe();
  // 下一次 create 不能失败：退避期内由 TS 接住，退避结束后由重启的 harness-host 接住。
  const next = await openTerminal(client, { cols: 80, rows: 24, cwd });
  const servedByTs = !String(next.id).startsWith("rpty-");
  await waitQuiet(next);
  const echo = await echoRoundtrip(next, "TS", "OK");
  await closeTerm(next);
  expect("B9", echo.seen, "terminal created after the crash does not work");
  return { exitCode: term.exitCode, nextServedBy: servedByTs ? "ts (backoff)" : "rust (restarted)" };
}
