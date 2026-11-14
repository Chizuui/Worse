// Renderer helpers for the Phase 7 parity gate. Both apps run the same packages/ui renderer, so the
// selectors are shared (packages/shared/src/test-ids.ts). Every wait is a Playwright DOM wait.
export const PANE = '[data-testid="v4-session-pane-workspace-main"]';
export const TIMELINE = `${PANE} [data-testid="v4-timeline"]`;
export const COMPOSER_INPUT = `${PANE} [data-testid="v4-composer-input"]`;
export const STOP = `${PANE} [data-testid="v4-stop"]`;
export const QUEUE_ITEM = `${PANE} li[data-testid^="v4-queue-item-"][data-queue-item-id]`;
// 用 :is() 包住两种语言的标签：直接写成逗号选择器列表时，后面拼接的后代选择器只作用于最后一项。
export const PERMISSION = ':is([role="listbox"][aria-label="Permission required"], [role="listbox"][aria-label="需要权限"])';

export const tid = (id) => `[data-testid="${id}"]`;

/** Dismisses the occupation onboarding (as ZCode's dismissOccupationOnboarding) and waits for the composer. */
export async function waitForWorkspace(page, timeout = 120_000) {
  await page.waitForSelector(`${tid("onboarding-page")}, ${COMPOSER_INPUT}`, { timeout });
  if (await page.locator(tid("onboarding-page")).count()) {
    await page.keyboard.press("Escape");
  }
  await page.waitForSelector(COMPOSER_INPUT, { timeout });
}

export async function sessionId(page) {
  return page.locator(PANE).first().getAttribute("data-session-id");
}

/** Types `text` into the composer and submits it with Enter; resolves when the composer is empty again. */
export async function send(page, text, timeout = 30_000) {
  const input = page.locator(COMPOSER_INPUT).first();
  await input.click();
  await page.keyboard.type(text);
  await page.waitForFunction(
    ({ selector, expected }) => document.querySelector(selector)?.textContent?.includes(expected),
    { selector: COMPOSER_INPUT, expected: text },
    { timeout },
  );
  await page.keyboard.press("Enter");
  await page.waitForFunction((selector) => (document.querySelector(selector)?.textContent ?? "").trim() === "", COMPOSER_INPUT, {
    timeout,
  });
}

export async function timelineText(page) {
  return page.evaluate((selector) => document.querySelector(selector)?.innerText ?? "", TIMELINE);
}

/** Waits until the active session timeline contains `text`. */
export async function waitTimeline(page, text, timeout = 60_000) {
  await page.waitForFunction(
    ({ selector, expected }) => (document.querySelector(selector)?.innerText ?? "").includes(expected),
    { selector: TIMELINE, expected: text },
    { timeout },
  );
}

export async function waitGone(page, selector, timeout = 30_000) {
  await page.waitForFunction((target) => !document.querySelector(target), selector, { timeout });
}

/** Opens a new draft session and waits for the pane to switch to it. */
export async function newSession(page, timeout = 30_000) {
  await page.locator(tid("task-new-button")).first().click();
  await page.waitForFunction((selector) => document.querySelector(selector)?.getAttribute("data-session-id") === "draft", PANE, {
    timeout,
  });
  await page.waitForSelector(COMPOSER_INPUT, { timeout });
}

/** Waits until the pane leaves the draft state and returns the new session id. */
export async function waitSessionCreated(page, timeout = 60_000) {
  await page.waitForFunction(
    (selector) => {
      const id = document.querySelector(selector)?.getAttribute("data-session-id");
      return Boolean(id) && id !== "draft";
    },
    PANE,
    { timeout },
  );
  return sessionId(page);
}

export async function openSession(page, id, timeout = 30_000) {
  await page.locator(tid(`task-item-${id}`)).first().click();
  await page.waitForFunction(
    ({ selector, expected }) => document.querySelector(selector)?.getAttribute("data-session-id") === expected,
    { selector: PANE, expected: id },
    { timeout },
  );
}

export async function taskItemIds(page) {
  return page.evaluate(() =>
    [...document.querySelectorAll('[data-testid^="task-item-"]')]
      .map((element) => element.getAttribute("data-testid").slice("task-item-".length))
      .filter((id, index, all) => id && all.indexOf(id) === index),
  );
}

/** Answers the permission prompt with the option matching kindSelector. */
export async function answerPermission(page, kindSelector, timeout = 30_000) {
  await page.waitForSelector(PERMISSION, { timeout });
  // 点击未选中的选项会先选中它（mousedown 聚焦触发 onFocus 选中，click 可能直接提交）；
  // 若提示仍在，确认目标项已选中后点 Confirm 提交。
  const target = `${PERMISSION} button[role="option"]${kindSelector}`;
  await page.locator(target).first().click();
  const confirmSelector = '[data-v4-composer-dock="true"] :is(button[aria-label="Confirm"], button[aria-label="确认"])';
  // 等到确定状态：提示已消失（点击默认项 allowOnce 会直接提交），或目标项已选中且 Confirm 在场。
  const state = await page
    .waitForFunction(
      ({ prompt, selector, confirm }) => {
        if (!document.querySelector(prompt)) return "gone";
        return document.querySelector(selector)?.getAttribute("aria-selected") === "true" && document.querySelector(confirm) ? "confirm" : false;
      },
      { prompt: PERMISSION, selector: target, confirm: confirmSelector },
      { timeout },
    )
    .then((handle) => handle.jsonValue());
  if (state === "confirm") {
    // 修复：此前用 locator.click 与“提示消失”竞速，输掉的点击不会取消，会在下一个权限提示出现时点到它的
    // Confirm（以默认 allowOnce 批准了 S5 的拒绝场景）。改为点击绑定到当前这个节点的 ElementHandle：
    // 节点脱离 DOM 时立即失败，不会落到之后的提示上。
    const handle = await page.$(confirmSelector);
    await handle?.click({ timeout: 5_000 }).catch(() => {});
    await handle?.dispose();
  }
  await waitGone(page, PERMISSION, timeout);
}

/** Text of the visible xterm rows (ZCode r3 terminal e2e reads `.xterm-rows` the same way). */
export async function terminalText(page) {
  return page.evaluate(() => {
    const rows = [...document.querySelectorAll('[data-testid="terminal"] .xterm-rows')].filter((element) => element.getClientRects().length > 0);
    const last = rows.at(-1);
    if (!last) return "";
    return [...last.children].map((row) => (row.textContent ?? "").replaceAll(" ", " ").trimEnd()).join("\n");
  });
}

export async function htmlTheme(page) {
  return page.evaluate(() => ({
    classes: [...document.documentElement.classList].filter((name) => name === "dark" || name.startsWith("theme-")),
    stored: (() => {
      try {
        return localStorage.getItem("zcode-theme");
      } catch {
        return null;
      }
    })(),
  }));
}
