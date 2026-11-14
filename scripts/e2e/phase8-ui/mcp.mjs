// Minimal client for Slint's embedded MCP introspection server (feature `slint/mcp`, env SLINT_MCP_PORT).
// JSON-RPC over HTTP POST http://127.0.0.1:<port>/mcp. Only loopback; the gate starts the app itself.

export class SlintMcp {
  constructor(port) {
    this.url = `http://127.0.0.1:${port}/mcp`;
    this.nextId = 1;
    this.window = null;
    this.root = null;
  }

  async rpc(method, params = {}) {
    const id = this.nextId++;
    const response = await fetch(this.url, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ jsonrpc: "2.0", id, method, params }),
      signal: AbortSignal.timeout(15_000),
    });
    const body = await response.json();
    if (body.error) throw new Error(`mcp ${method}: ${body.error.message}`);
    return body.result;
  }

  /** Calls a tool; returns parsed JSON (text tools) or `{ png: Buffer }` (screenshots). */
  async tool(name, args = {}) {
    const result = await this.rpc("tools/call", { name, arguments: args });
    const content = result?.content ?? [];
    if (result?.isError) throw new Error(`mcp tool ${name}: ${content.map((c) => c.text).join(" ")}`);
    const image = content.find((c) => c.type === "image");
    if (image) return { png: Buffer.from(image.data, "base64") };
    const text = content.find((c) => c.type === "text")?.text ?? "{}";
    try {
      return JSON.parse(text);
    } catch {
      return { text };
    }
  }

  /** Waits until the server answers and the main window exists. */
  async connect(timeoutMs = 30_000) {
    const deadline = Date.now() + timeoutMs;
    let lastError = null;
    while (Date.now() < deadline) {
      try {
        await this.rpc("initialize", { protocolVersion: "2025-03-26", capabilities: {}, clientInfo: { name: "phase8-ui-gate", version: "1" } });
        const windows = await this.tool("list_windows");
        const handle = windows.windowHandles?.[0];
        if (handle) {
          this.window = handle;
          const props = await this.tool("get_window_properties", { windowHandle: handle });
          this.root = props.rootElementHandle;
          if (this.root) return props;
        }
      } catch (error) {
        lastError = error;
      }
      await sleep(250);
    }
    throw new Error(`MCP server not ready: ${lastError?.message ?? "no window"}`);
  }

  async elements() {
    const tree = await this.tool("get_element_tree", { elementHandle: this.root, maxElements: 1000 });
    if (tree.debugInfoMissing) throw new Error("element tree has no debug info (build with --features automation)");
    return tree.elements ?? [];
  }

  async find(predicate) {
    return (await this.elements()).find(predicate) ?? null;
  }

  /** Polls the tree until `predicate` matches an element; returns it. */
  async waitFor(what, predicate, timeoutMs = 20_000) {
    const deadline = Date.now() + timeoutMs;
    while (Date.now() < deadline) {
      const hit = await this.find(predicate);
      if (hit) return hit;
      await sleep(100);
    }
    throw new Error(`timeout waiting for ${what}`);
  }

  async waitGone(what, predicate, timeoutMs = 20_000) {
    const deadline = Date.now() + timeoutMs;
    while (Date.now() < deadline) {
      if (!(await this.find(predicate))) return;
      await sleep(100);
    }
    throw new Error(`timeout waiting for ${what} to disappear`);
  }

  click(element) {
    return this.tool("click_element", { elementHandle: element.handle });
  }

  activate(element) {
    return this.tool("invoke_accessibility_action", { elementHandle: element.handle, action: "Default_" });
  }

  setValue(element, value) {
    return this.tool("set_element_value", { elementHandle: element.handle, value });
  }

  key(text, eventType = "PressAndRelease") {
    return this.tool("dispatch_key_event", { windowHandle: this.window, text, eventType });
  }

  /** Wheel scroll at a logical window position (positive deltaY scrolls content down = towards the top). */
  scrollAt(x, y, deltaY) {
    return this.tool("dispatch_pointer_scroll", { windowHandle: this.window, position: { x, y }, deltaX: 0, deltaY });
  }

  async screenshot() {
    return (await this.tool("take_screenshot", { windowHandle: this.window })).png;
  }
}

export const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

export const label = (element) => element.accessibleLabel ?? "";
export const role = (element) => element.accessibleRole ?? "";
export const byLabel = (text) => (element) => label(element) === text;
export const byLabelPrefix = (prefix, includes = "") => (element) => label(element).startsWith(prefix) && label(element).includes(includes);

/** 8.2 遗留 bug 的回归断言：可见聊天行（ChatRow）按 y 排序后，相邻行不得重叠。 */
export async function assertNoRowOverlap(mcp) {
  const rows = (await mcp.elements())
    .filter((e) => (e.typeNamesAndIds ?? []).some((t) => t.typeName === "ChatRow"))
    .map((e) => ({ y: e.absolutePosition?.y ?? 0, h: e.size?.height ?? 0 }))
    .sort((a, b) => a.y - b.y);
  if (rows.length < 2) throw new Error(`expected at least 2 chat rows with geometry, got ${rows.length} (automation build?)`);
  for (let i = 0; i + 1 < rows.length; i++) {
    if (rows[i].y + rows[i].h > rows[i + 1].y + 0.5) {
      throw new Error(`chat row ${i} (y ${rows[i].y}, h ${rows[i].h}) overlaps row ${i + 1} (y ${rows[i + 1].y})`);
    }
  }
  return rows.length;
}

