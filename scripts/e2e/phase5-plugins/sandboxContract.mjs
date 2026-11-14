// P1 + P2 + P3 of the Phase 5 plugins gate: the Harness sandbox owner against ZCode Main's registry and
// protocol handler, in process (spec docs/specs/harness/phase-5-plugins.md, section 7).
import { readFile } from "node:fs/promises";
import path from "node:path";
import { fail, withTimeout } from "../phase2/gate.mjs";
import {
  CALL_TIMEOUT_MS,
  EMBEDDER,
  assertGuestDocument,
  assetsDir,
  cspDirectives,
  decode,
  h32Of,
  headerMap,
  hsandboxUrl,
  runDir,
  shellBundle,
  winOrigin,
} from "./common.mjs";

function registerRequest(overrides = {}) {
  return {
    type: "plugin-sandbox-register-request",
    requestId: `req-${Math.random().toString(16).slice(2)}`,
    ownerWebContentsId: 1,
    sessionId: "phase5-session",
    workspacePath: path.join(runDir, "workspace"),
    scopeId: "tool:phase5",
    pluginId: "phase5-hello@phase5-fixtures",
    serverName: "phase5",
    instance: { runtimeId: "phase5-runtime", generation: 1, appIdentity: "ab".repeat(32), token: "phase5-token" },
    html: '<!doctype html><html><head><title>fixture</title></head><body><p id="hello">hello plugin</p></body></html>',
    csp: { connectDomains: ["https://api.phase5.test"], resourceDomains: ["https://cdn.phase5.test"] },
    permissions: ["clipboardWrite", "camera"],
    ...overrides,
  };
}

/** Harness owner with promise-based register / call helpers (the same shapes main.ts wires). */
function createOwnerHarness(ownerExports, platform = "win32") {
  const toHost = [];
  const replies = new Map();
  let nextId = 1;
  const hostWaiters = new Map();
  const owner = ownerExports.createPluginSandboxOwner({
    postToHost: (message) => {
      toHost.push(message);
      hostWaiters.get(message.requestId)?.(message);
    },
    postToShell: (message) => replies.get(message.id)?.(message),
    assetsDir,
    platform,
  });
  const register = (request) =>
    withTimeout(
      new Promise((resolve) => {
        hostWaiters.set(request.requestId, resolve);
        if (!owner.interceptHostMessage(request)) fail("P2_INTERCEPT", "the owner did not consume a register request");
      }),
      CALL_TIMEOUT_MS,
      "P2_REGISTER",
      "plugin-sandbox-register-result",
    );
  const call = (method, params) =>
    withTimeout(
      new Promise((resolve) => {
        const id = `gate-${nextId++}`;
        replies.set(id, (message) => {
          replies.delete(id);
          resolve(message);
        });
        owner.handleShellMessage({ type: "harness-web-remote-sandbox-call", id, method, params });
      }),
      CALL_TIMEOUT_MS,
      "P2_CALL",
      `sandbox ${method}`,
    );
  const fetchUrl = async (url, method = "GET", embedderOrigin = EMBEDDER) => {
    const reply = await call("fetch", { url, method, embedderOrigin });
    if (!reply.ok) return { error: reply.error };
    return reply.value;
  };
  return { owner, toHost, register, call, fetchUrl };
}

/** ZCode Main's registry + protocol handler, unchanged (the reference side of P1). */
function createZcodeReference(vendor) {
  const registry = vendor.createPluginSandboxRegistry();
  const shellStub = "<!doctype html><title>zcode-shell</title>";
  const handle = vendor.createPluginSandboxProtocolHandler({
    registry,
    assets: { rendererDir: path.join(runDir, "zcode-renderer"), aliasDir: assetsDir },
    // ZCode 的 shell HTML 是 vite 产物，未随仓库构建；参考端用占位文档，只比较响应头。
    readAsset: async (absolutePath) =>
      absolutePath.endsWith("plugin-sandbox.html") ? new TextEncoder().encode(shellStub) : new Uint8Array(await readFile(absolutePath)),
    logger: { warn: () => undefined, info: () => undefined },
  });
  const fetchUrl = async (url, method = "GET") => {
    const response = await handle(new Request(url, { method }));
    return {
      status: response.status,
      headers: [...response.headers.entries()],
      body: Buffer.from(await response.arrayBuffer()),
    };
  };
  return { registry, fetchUrl };
}

/** Compares two header sets; `allowedCsp` names CSP directives Harness may legitimately rewrite. */
function compareHeaders(step, harnessPairs, zcodePairs, allowedCsp, expectedCsp) {
  const harness = headerMap(harnessPairs);
  const zcode = headerMap(zcodePairs);
  for (const key of new Set([...Object.keys(harness), ...Object.keys(zcode)])) {
    if (key === "content-security-policy") continue;
    if (harness[key] !== zcode[key]) fail(step, `header ${key}: harness=${harness[key]} zcode=${zcode[key]}`);
  }
  const hd = cspDirectives(harness["content-security-policy"]);
  const zd = cspDirectives(zcode["content-security-policy"]);
  for (const name of new Set([...Object.keys(hd), ...Object.keys(zd)])) {
    if (allowedCsp.includes(name)) continue;
    if (hd[name] !== zd[name]) fail(step, `CSP ${name}: harness=${hd[name]} zcode=${zd[name]}`);
  }
  for (const [name, value] of Object.entries(expectedCsp)) {
    if (hd[name] !== value) fail(step, `CSP ${name} must be "${value}", got "${hd[name]}"`);
  }
  return { headers: Object.keys(harness).length, rewrittenCsp: allowedCsp };
}

export async function runSandboxContract(vendor, ownerExports) {
  const summary = {};
  const harness = createOwnerHarness(ownerExports);
  const zcode = createZcodeReference(vendor);
  try {
    // --- register (P2) ---
    const request = registerRequest();
    const reply = await harness.register(request);
    const { type: _t, requestId: _r, ...zcodeInput } = request;
    const reference = zcode.registry.register(zcodeInput);
    if (reply.type !== "plugin-sandbox-register-result" || reply.ok !== true || reply.requestId !== request.requestId) {
      fail("P2_REGISTER", `unexpected register reply ${JSON.stringify(reply).slice(0, 300)}`);
    }
    const resultCheck = vendor.hostPluginSandboxRegisterResultMessageSchema?.safeParse?.(reply);
    if (resultCheck && !resultCheck.success) fail("P2_REGISTER_SCHEMA", "the register reply fails ZCode's strict result schema");
    const { appIdentity } = request.instance;
    const expectedShell = `${winOrigin("shell", appIdentity)}/instance/${reply.sandboxId}/#init=${reply.initId}`;
    if (reply.shellUrl !== expectedShell) fail("P2_SHELL_URL", `shellUrl ${reply.shellUrl} != ${expectedShell}`);
    // 与 ZCode 句柄逐字段对比：除 sandboxId（随机）与 shellUrl（Harness 源）外必须一致。
    for (const key of ["initId", "partition", "resourceMeta", "instance"]) {
      if (JSON.stringify(reply[key]) !== JSON.stringify(reference[key])) {
        fail("P1_HANDLE", `handle.${key}: harness=${JSON.stringify(reply[key])} zcode=${JSON.stringify(reference[key])}`);
      }
    }
    if (!/^sb-[a-f0-9]+$/.test(reply.sandboxId)) fail("P1_HANDLE", `sandboxId ${reply.sandboxId} does not use ZCode's format`);
    summary.register = { shellUrl: reply.shellUrl.replace(reply.sandboxId, "<id>"), partitionMatches: true };

    // idempotency: same key → same sandbox (ZCode registry semantics)
    const again = await harness.register({ ...request, requestId: "req-again" });
    const referenceAgain = zcode.registry.register(zcodeInput);
    const harnessIdempotent = again.sandboxId === reply.sandboxId;
    const zcodeIdempotent = referenceAgain.sandboxId === reference.sandboxId;
    if (harnessIdempotent !== zcodeIdempotent) fail("P1_IDEMPOTENT", `harness=${harnessIdempotent} zcode=${zcodeIdempotent}`);
    summary.idempotentReRegister = harnessIdempotent;
    const live = { id: again.sandboxId, initId: again.initId };
    const refLive = { id: referenceAgain.sandboxId };

    // --- documents: plugin index, alias script, shell (P1) ---
    const pluginRest = (id) => `/instance/${id}/index.html`;
    const hPlugin = await harness.fetchUrl(hsandboxUrl("plugin", appIdentity, pluginRest(live.id)));
    const zPlugin = await zcode.fetchUrl(`zcode-sandbox://plugin-${appIdentity}${pluginRest(refLive.id)}`);
    if (hPlugin.status !== 200 || zPlugin.status !== 200) fail("P1_PLUGIN", `status harness=${hPlugin.status} zcode=${zPlugin.status}`);
    compareHeaders("P1_PLUGIN_HEADERS", hPlugin.headers, zPlugin.headers, ["frame-ancestors"], {
      "frame-ancestors": `frame-ancestors ${winOrigin("shell", appIdentity)} ${EMBEDDER}`,
    });
    const hBody = decode(hPlugin).toString("utf8").replaceAll(live.id, "<id>");
    const zBody = zPlugin.body.toString("utf8").replaceAll(refLive.id, "<id>");
    if (hBody !== zBody) fail("P1_PLUGIN_BODY", `plugin document differs:\n harness=${hBody.slice(0, 300)}\n zcode=${zBody.slice(0, 300)}`);
    assertGuestDocument("P3_PLUGIN", hPlugin);

    const aliasPath = /src="([^"]*__zcode__[^"]*)"/.exec(hBody)?.[1]?.replace("<id>", live.id);
    if (!aliasPath) fail("P1_ALIAS", "plugin document does not inject ZCode's alias script");
    const hAlias = await harness.fetchUrl(hsandboxUrl("plugin", appIdentity, aliasPath));
    const zAlias = await zcode.fetchUrl(`zcode-sandbox://plugin-${appIdentity}${aliasPath.replace(live.id, refLive.id)}`);
    if (hAlias.status !== 200 || !decode(hAlias).equals(zAlias.body)) fail("P1_ALIAS", `alias script differs (status ${hAlias.status}/${zAlias.status})`);
    compareHeaders("P1_ALIAS_HEADERS", hAlias.headers, zAlias.headers, ["frame-ancestors"], {});
    summary.pluginDocument = { identicalBody: true, aliasBytes: zAlias.body.length };

    const hShell = await harness.fetchUrl(hsandboxUrl("shell", appIdentity, `/instance/${live.id}/`));
    const zShell = await zcode.fetchUrl(`zcode-sandbox://shell-${appIdentity}/instance/${refLive.id}/`);
    if (hShell.status !== 200 || zShell.status !== 200) fail("P1_SHELL", `status harness=${hShell.status} zcode=${zShell.status}`);
    compareHeaders("P1_SHELL_HEADERS", hShell.headers, zShell.headers, ["frame-src", "frame-ancestors"], {
      "frame-src": `frame-src ${winOrigin("plugin", appIdentity)}`,
      "frame-ancestors": `frame-ancestors ${EMBEDDER}`,
      "connect-src": "connect-src 'none'",
      "default-src": "default-src 'none'",
    });
    assertGuestDocument("P3_SHELL", hShell);
    const shellHtml = decode(hShell).toString("utf8");
    const meta = /<meta name="harness-sandbox" content="([^"]*)"/.exec(shellHtml)?.[1];
    if (!meta) fail("P2_SHELL_META", "Harness shell document lacks its config");
    const config = JSON.parse(meta.replaceAll("&quot;", '"').replaceAll("&lt;", "<").replaceAll("&amp;", "&"));
    const expectedAllow = vendor.buildMcpAppsIframeAllow(request.permissions);
    if (config.sandboxId !== live.id || config.embedderOrigin !== EMBEDDER || config.allow !== expectedAllow) {
      fail("P2_SHELL_META", `shell config ${JSON.stringify(config)} (expected allow "${expectedAllow}")`);
    }
    if (config.pluginOrigin !== winOrigin("plugin", appIdentity)) fail("P2_SHELL_META", `plugin origin ${config.pluginOrigin}`);
    const scriptSrc = /<script[^>]*src="([^"]+)"/.exec(shellHtml)?.[1];
    const hShellJs = await harness.fetchUrl(hsandboxUrl("shell", appIdentity, `/instance/${live.id}/${scriptSrc}`));
    if (hShellJs.status !== 200 || !decode(hShellJs).equals(await readFile(shellBundle))) fail("P2_SHELL_JS", `shell script status ${hShellJs.status}`);
    assertGuestDocument("P3_SHELL_JS", hShellJs);
    const shellJs = decode(hShellJs).toString("utf8");
    // shell 只接受来自 embedder 的 init（event.source === window.parent 且 origin 固定），并用 meta 的 allow。
    for (const needle of ["event.source === window.parent", "event.origin === config.embedderOrigin", "allow: config.allow", "event.origin === config.pluginOrigin"]) {
      if (!shellJs.includes(needle)) fail("P3_SHELL_INIT", `shell script lacks the guard \`${needle}\``);
    }
    summary.shellDocument = { csp: "ZCode shell CSP; frame-src → harness plugin origin; + frame-ancestors embedder", allow: config.allow };

    // --- isolation (P2) ---
    const statuses = {};
    const expectStatus = async (label, url, status, method = "GET") => {
      const value = await harness.fetchUrl(url, method);
      statuses[label] = value.status;
      if (value.status !== status) fail(`P2_${label}`, `${url} → ${value.status}, expected ${status}`);
      if (status !== 200 && headerMap(value.headers)["cache-control"] !== "no-store") fail(`P2_${label}`, "error response must be no-store");
    };
    const other = "cd".repeat(32);
    await expectStatus("WRONG_H32", hsandboxUrl("plugin", other, pluginRest(live.id)), 404);
    await expectStatus("UNKNOWN_SANDBOX", hsandboxUrl("plugin", appIdentity, pluginRest("sb-0000")), 404);
    await expectStatus("SHORT_HOST", `hsandbox://plugin-${appIdentity.slice(0, 31)}.localhost${pluginRest(live.id)}`, 404);
    await expectStatus("FOREIGN_SCHEME", `http://plugin-${h32Of(appIdentity)}.localhost${pluginRest(live.id)}`, 404);
    await expectStatus("NO_INSTANCE", hsandboxUrl("plugin", appIdentity, "/index.html"), 404);
    await expectStatus("SHELL_OTHER_PATH", hsandboxUrl("shell", appIdentity, `/instance/${live.id}/../../etc/passwd`), 404);
    await expectStatus("SHELL_TRAVERSAL", hsandboxUrl("shell", appIdentity, `/instance/${live.id}/assets/..%2F..%2Fpackage.json`), 404);
    await expectStatus("POST", hsandboxUrl("plugin", appIdentity, pluginRest(live.id)), 405, "POST");
    const zPost = await zcode.fetchUrl(`zcode-sandbox://plugin-${appIdentity}${pluginRest(refLive.id)}`, "POST");
    if (zPost.status !== 405) fail("P1_POST", `ZCode answered POST with ${zPost.status}`);
    const badEmbedder = await harness.fetchUrl(hsandboxUrl("plugin", appIdentity, pluginRest(live.id)), "GET", "https://evil.example/x");
    if (!badEmbedder.error) fail("P2_EMBEDDER", "an invalid embedder origin was accepted");
    summary.isolation = statuses;

    // --- owner checks (P2) ---
    const foreign = await harness.register(registerRequest({ ownerWebContentsId: 2, requestId: "req-foreign" }));
    if (foreign.ok !== false || foreign.error !== "Plugin UI sandbox is unavailable.") fail("P2_OWNER", JSON.stringify(foreign));
    const invalid = await harness.register(registerRequest({ requestId: "req-invalid", serverName: undefined }));
    if (invalid.ok !== false) fail("P2_INVALID", "a registration without provenance was accepted");
    const leaked = harness.toHost.filter((message) => message.type !== "plugin-sandbox-register-result");
    if (leaked.length > 0) fail("P2_LEAK", "the owner posted non-result messages to the Host");
    summary.ownerChecks = { foreignOwner: foreign.error, missingProvenance: "rejected" };

    // --- quota (P1: same error as ZCode) ---
    const quota = createOwnerHarness(ownerExports);
    const quotaRef = vendor.createPluginSandboxRegistry();
    let harnessQuotaError = null;
    let zcodeQuotaError = null;
    for (let index = 0; index < 70 && (harnessQuotaError === null || zcodeQuotaError === null); index += 1) {
      const appId = index.toString(16).padStart(64, "0");
      const req = registerRequest({ requestId: `q-${index}`, scopeId: `tool:q${index}`, instance: { runtimeId: "q", generation: 1, appIdentity: appId, token: "q" } });
      const result = await quota.register(req);
      if (!result.ok && harnessQuotaError === null) harnessQuotaError = { index, error: result.error };
      if (zcodeQuotaError === null) {
        try {
          const { type: _a, requestId: _b, ...input } = req;
          quotaRef.register(input);
        } catch (error) {
          zcodeQuotaError = { index, error: error.message };
        }
      }
    }
    quota.owner.dispose();
    if (JSON.stringify(harnessQuotaError) !== JSON.stringify(zcodeQuotaError) || harnessQuotaError === null) {
      fail("P1_QUOTA", `harness=${JSON.stringify(harnessQuotaError)} zcode=${JSON.stringify(zcodeQuotaError)}`);
    }
    summary.quota = harnessQuotaError;

    // --- dispose (P2) ---
    await harness.call("dispose", { sandboxId: live.id, initId: live.initId + 1 });
    const stillThere = await harness.fetchUrl(hsandboxUrl("plugin", appIdentity, pluginRest(live.id)));
    if (stillThere.status !== 200) fail("P2_DISPOSE_STALE", "a stale-initId dispose removed the live registration");
    await harness.call("dispose", { sandboxId: live.id, initId: live.initId });
    const gone = await harness.fetchUrl(hsandboxUrl("plugin", appIdentity, pluginRest(live.id)));
    if (gone.status !== 404) fail("P2_DISPOSE", `disposed sandbox still served (${gone.status})`);
    summary.dispose = { staleInitIdKept: true, currentInitIdRemoved: true };

    // --- Gen UI (P1 + P2) ---
    const genUiIdentity = "ef".repeat(32);
    const genUiRequest = registerRequest({
      requestId: "req-genui",
      contentKind: "gen-ui",
      pluginId: undefined,
      serverName: undefined,
      permissions: undefined,
      scopeId: "genui:phase5",
      instance: { runtimeId: "gen-ui:phase5", generation: 1, appIdentity: genUiIdentity, token: "g" },
      html: '<!doctype html><html><head><meta charset="utf-8"><!--__ZCODE_GEN_UI_STYLES__--><script src="__ZCODE_GEN_UI_RUNTIME__"></script></head><body><div id="g">gen ui</div><!--__ZCODE_GEN_UI_INNER_KIT__--></body></html>',
      csp: { connectDomains: [], resourceDomains: ["https://unpkg.com", "https://cdn.jsdelivr.net"], frameDomains: [], baseUriDomains: [] },
    });
    for (const key of Object.keys(genUiRequest)) if (genUiRequest[key] === undefined) delete genUiRequest[key];
    const genUi = await harness.register(genUiRequest);
    if (!genUi.ok) fail("P2_GENUI", `gen-ui registration failed: ${genUi.error}`);
    const { type: _gt, requestId: _gr, ...genUiInput } = genUiRequest;
    const genUiRef = zcode.registry.register(genUiInput);
    const hGen = await harness.fetchUrl(hsandboxUrl("plugin", genUiIdentity, pluginRest(genUi.sandboxId)));
    const zGen = await zcode.fetchUrl(`zcode-sandbox://plugin-${genUiIdentity}${pluginRest(genUiRef.sandboxId)}`);
    compareHeaders("P1_GENUI_HEADERS", hGen.headers, zGen.headers, ["frame-ancestors"], {});
    const manifest = JSON.parse(await readFile(path.join(assetsDir, "vendor/manifest.json"), "utf8"));
    let zGenBody = zGen.body.toString("utf8").replaceAll(genUiRef.sandboxId, "<id>");
    let rewritten = 0;
    for (const { url, file } of manifest.resources ?? []) {
      if (zGenBody.includes(url)) rewritten += 1;
      zGenBody = zGenBody.replaceAll(url, `/instance/<id>/__zcode__/vendor/${file}`);
    }
    const hGenBody = decode(hGen).toString("utf8").replaceAll(genUi.sandboxId, "<id>");
    if (hGenBody !== zGenBody) fail("P1_GENUI_BODY", "Gen UI document differs from ZCode's beyond the vendor URL rewrite");
    assertGuestDocument("P3_GENUI", hGen);
    let vendorFetched = null;
    const vendorRef = /\/instance\/<id>\/__zcode__\/vendor\/[^"']+/.exec(hGenBody)?.[0];
    if (vendorRef) {
      const vendorFile = await harness.fetchUrl(hsandboxUrl("plugin", genUiIdentity, vendorRef.replace("<id>", genUi.sandboxId)));
      if (vendorFile.status !== 200) fail("P2_GENUI_VENDOR", `${vendorRef} → ${vendorFile.status}`);
      vendorFetched = vendorRef.split("/").pop();
    }
    summary.genUi = { identicalAfterVendorRewrite: true, vendorUrlsRewritten: rewritten, vendorFetched };

    // --- macOS/Linux origins (P2) ---
    const posix = createOwnerHarness(ownerExports, "linux");
    const posixReply = await posix.register(registerRequest({ requestId: "req-posix" }));
    posix.owner.dispose();
    if (!posixReply.shellUrl.startsWith(`hsandbox://shell-${h32Of(appIdentity)}.localhost/instance/`)) {
      fail("P2_POSIX_URL", `posix shellUrl ${posixReply.shellUrl}`);
    }

    // --- shutdown (P2) ---
    harness.owner.dispose();
    const afterDispose = await harness.call("stats", {});
    if (afterDispose.ok !== false) fail("P2_SHUTDOWN", "the owner kept answering after dispose");
    return summary;
  } finally {
    harness.owner.dispose();
  }
}
