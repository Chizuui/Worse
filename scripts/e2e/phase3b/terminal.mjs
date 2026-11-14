// Terminal helpers for the Phase 3b gate: drive `terminal` over a real @zcode/rpc client exactly as
// the renderer does (create -> onDynamicData/onDynamicExit -> write/resize/dispose).

const OUTPUT_TIMEOUT_MS = 20_000;
const ESC = String.fromCharCode(0x1b);
const BEL = String.fromCharCode(0x07);
/** CSI, OSC (BEL or ST terminated) and two-byte escapes. */
const ANSI = new RegExp(`${ESC}\\[[0-?]*[ -/]*[@-~]|${ESC}\\][^${BEL}${ESC}]*(?:${BEL}|${ESC}\\\\)|${ESC}[@-_]`, "g");

export function stripAnsi(text) {
  return text.replace(ANSI, "");
}

/**
 * Shell-specific commands whose *output* differs from the typed text, so a marker found in the
 * output proves execution (the PTY also echoes what we type).
 */
export function shellCommands(shell) {
  const name = shell.toLowerCase().replace(/\\/g, "/").split("/").pop() ?? "";
  if (name.startsWith("pwsh") || name.startsWith("powershell")) {
    return {
      kind: "powershell",
      print: (a, b) => `Write-Output ("${a}" + "${b}")`,
      width: () => `Write-Output ("W=" + [Console]::WindowWidth)`,
      eol: "\r",
    };
  }
  if (name.startsWith("cmd")) {
    return { kind: "cmd", print: (a, b) => `echo ${a}^${b}`, width: null, eol: "\r" };
  }
  return {
    kind: "sh",
    print: (a, b) => `printf '%s%s\\n' '${a}' '${b}'`,
    width: () => `printf 'W=%s\\n' "$(stty size | cut -d' ' -f2)"`,
    eol: "\n",
  };
}

/** Creates a terminal and subscribes like the renderer; returns a small handle. */
export async function openTerminal(client, params) {
  const channel = client.getChannel("terminal");
  const startedAt = performance.now();
  const created = await channel.call("create", [params]);
  const spawnMs = performance.now() - startedAt;
  const term = {
    created,
    id: created.id,
    spawnMs,
    raw: "",
    exitCode: undefined,
    exitAt: undefined,
    subscriptions: [],
    /** Called synchronously on every data chunk (event-driven waits, no timer quantisation). */
    dataWaiters: new Set(),
    commands: shellCommands(created.shell),
  };
  term.subscriptions.push(
    channel.listen("onDynamicData", created.id)((data) => {
      term.raw += data;
      for (const waiter of term.dataWaiters) waiter(data);
    }),
    channel.listen("onDynamicExit", created.id)((code) => {
      term.exitCode = code;
      term.exitAt = performance.now();
    }),
  );
  term.text = () => stripAnsi(term.raw);
  term.write = (data) => channel.call("write", [{ id: created.id, data }]);
  term.run = (line) => term.write(`${line}${term.commands.eol}`);
  term.resize = (cols, rows) => channel.call("resize", [{ id: created.id, cols, rows }]);
  term.dispose = () => channel.call("dispose", [{ id: created.id }]);
  term.unsubscribe = () => {
    for (const subscription of term.subscriptions.splice(0)) subscription.dispose();
  };
  return term;
}

export async function waitUntil(predicate, timeoutMs = OUTPUT_TIMEOUT_MS) {
  const deadline = performance.now() + timeoutMs;
  while (!predicate()) {
    if (performance.now() > deadline) return false;
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  return true;
}

/** Waits for the shell prompt to settle (no output for `quietMs`) so typed input is not lost. */
export async function waitQuiet(term, quietMs = 400, timeoutMs = OUTPUT_TIMEOUT_MS) {
  const deadline = performance.now() + timeoutMs;
  let last = term.raw.length;
  let stableSince = performance.now();
  while (performance.now() < deadline) {
    await new Promise((resolve) => setTimeout(resolve, 25));
    if (term.raw.length !== last) {
      last = term.raw.length;
      stableSince = performance.now();
    } else if (term.raw.length > 0 && performance.now() - stableSince >= quietMs) {
      return true;
    }
  }
  return false;
}

/**
 * Runs a print command and measures write -> marker visible in the output.
 *
 * 修复原因（Phase 3b 回显延迟误判）：
 * 1. 旧实现用 waitUntil 每 10ms 轮询，而 Windows 上 Node 定时器按 ~15.6ms 系统时钟节拍触发，测得的值被
 *    量化到节拍网格上，且网格起点取决于 write 响应何时返回，0.5ms 的真实差异就可能被放大成一整个节拍。
 *    改为在数据回调里同步判定标记，计时精确到事件到达。
 * 2. 判定只扫描本轮写入之后的输出：对整个终端历史反复 stripAnsi 是 O(累计输出) 的，每块输出都要付一次，
 *    输出分块更细的一方（Rust 链路约 80 块/行，node-pty 约 50 块/行）会被测量本身拖慢。
 * 同时记录 write 应答与第一块输出的到达时间，供定位延迟分布使用。
 */
export async function echoRoundtrip(term, a, b, timeoutMs = OUTPUT_TIMEOUT_MS) {
  const marker = `${a}${b}`;
  const from = term.raw.length;
  const timing = { firstDataMs: null, writeAckMs: null };
  let resolveSeen;
  const seen = new Promise((resolve) => {
    resolveSeen = resolve;
  });
  const startedAt = performance.now();
  const waiter = (data) => {
    const at = performance.now() - startedAt;
    if (process.env.PHASE3B_ECHO_TRACE) process.stderr.write(`ECHOTRACE ${marker} +${at.toFixed(2)} ${JSON.stringify(data)}\n`);
    if (timing.firstDataMs === null) timing.firstDataMs = at;
    // 打印命令拼出标记（输入回显里没有完整标记），所以本轮输出里出现标记即命令已执行。
    if (stripAnsi(term.raw.slice(from)).includes(marker)) resolveSeen(at);
  };
  term.dataWaiters.add(waiter);
  let timer;
  try {
    await term.run(term.commands.print(a, b));
    timing.writeAckMs = performance.now() - startedAt;
    const ms = await Promise.race([
      seen,
      new Promise((resolve) => {
        timer = setTimeout(() => resolve(null), timeoutMs);
      }),
    ]);
    return ms === null ? { seen: false, ms: performance.now() - startedAt, ...timing } : { seen: true, ms, ...timing };
  } finally {
    clearTimeout(timer);
    term.dataWaiters.delete(waiter);
  }
}
