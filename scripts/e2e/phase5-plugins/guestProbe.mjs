// P6 guest probe for the Phase 5 plugins gate: `probeGuest` is serialised into the shell and plugin frames
// (it must stay self-contained); `assertGuestProbe` checks what a guest could reach.
import { fail } from "../phase2/gate.mjs";

/** Runs inside a guest frame (shell or plugin); returns what the guest can reach. */
export async function probeGuest() {
  const result = {
    origin: location.origin,
    tauriInternals: typeof window.__TAURI_INTERNALS__,
    tauri: typeof window.__TAURI__,
    zcode: typeof window.zcode,
    ipcGlobal: typeof window.ipc,
  };
  try {
    void window.top.document.title;
    result.topDocument = "readable";
  } catch {
    result.topDocument = "blocked";
  }
  try {
    void window.parent.location.href;
    result.parentLocation = "readable";
  } catch {
    result.parentLocation = "blocked";
  }
  const settle = (promise) =>
    Promise.race([promise.then(() => "allowed", (error) => `blocked:${error?.name ?? "error"}`), new Promise((resolve) => setTimeout(() => resolve("timeout"), 5_000))]);
  result.ipcFetch = await settle(fetch("http://ipc.localhost/plugin%3Awindow%7Cclose", { method: "POST", body: "{}" }));
  result.ipcSchemeFetch = await settle(fetch("ipc://localhost/plugin_sandbox_call", { method: "POST", body: "{}" }));
  result.chromeWebview = typeof window.chrome?.webview;
  // wry 在 WebView2 上把 Tauri 初始化脚本注入所有帧（忽略 for_main_frame_only），这里实际调用一次无副作用命令：
  // Tauri invoke 先走 ipc.localhost fetch，失败后回落到 postMessage，两条通道都必须拿不到结果。
  result.invokeHostStatus = typeof window.__TAURI_INTERNALS__?.invoke === "function"
    ? await settle(Promise.resolve().then(() => window.__TAURI_INTERNALS__.invoke("host_status")))
    : "blocked:absent";
  result.invokeOpenAttachment = typeof window.__TAURI_INTERNALS__?.invoke === "function"
    ? await settle(Promise.resolve().then(() => window.__TAURI_INTERNALS__.invoke("host_open_attachment")))
    : "blocked:absent";
  const policy = document.permissionsPolicy ?? document.featurePolicy;
  result.policy = policy
    ? Object.fromEntries(["camera", "microphone", "geolocation", "clipboard-write"].map((feature) => [feature, policy.allowsFeature(feature)]))
    : null;
  result.getUserMedia = navigator.mediaDevices?.getUserMedia
    ? await settle(navigator.mediaDevices.getUserMedia({ audio: true }))
    : "blocked:unavailable";
  result.geolocation = navigator.geolocation
    ? await Promise.race([
        new Promise((resolve) => navigator.geolocation.getCurrentPosition(() => resolve("allowed"), (error) => resolve(`blocked:${error.code}`))),
        new Promise((resolve) => setTimeout(() => resolve("timeout"), 5_000)),
      ])
    : "blocked:unavailable";
  result.text = document.body?.innerText?.slice(0, 200) ?? "";
  return result;
}

export function assertGuestProbe(step, probe, expectClipboard) {
  const problems = [];
  for (const key of ["tauri", "zcode"]) if (probe[key] !== "undefined") problems.push(`${key}=${probe[key]}`);
  // __TAURI_INTERNALS__ / window.ipc 由 wry 注入到子帧（WebView2 限制，spec 第 2 节 deviation）；判据是 IPC 不可用。
  for (const key of ["invokeHostStatus", "invokeOpenAttachment"]) if (probe[key] === "allowed") problems.push(`${key} succeeded`);
  if (probe.topDocument !== "blocked") problems.push("window.top.document readable");
  if (probe.parentLocation !== "blocked") problems.push("window.parent.location readable");
  if (!String(probe.ipcFetch).startsWith("blocked")) problems.push(`ipc.localhost fetch ${probe.ipcFetch}`);
  if (!String(probe.ipcSchemeFetch).startsWith("blocked")) problems.push(`ipc:// fetch ${probe.ipcSchemeFetch}`);
  if (!probe.policy) problems.push("no permissions policy API");
  else {
    for (const feature of ["camera", "microphone", "geolocation"]) if (probe.policy[feature] !== false) problems.push(`${feature} allowed by policy`);
    if (expectClipboard !== undefined && probe.policy["clipboard-write"] !== expectClipboard) problems.push(`clipboard-write=${probe.policy["clipboard-write"]}`);
  }
  if (!String(probe.getUserMedia).startsWith("blocked")) problems.push(`getUserMedia ${probe.getUserMedia}`);
  if (!String(probe.geolocation).startsWith("blocked")) problems.push(`geolocation ${probe.geolocation}`);
  if (problems.length > 0) fail(step, `${probe.origin}: ${problems.join("; ")}`);
}
