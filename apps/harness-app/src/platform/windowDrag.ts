import { getCurrentWindow } from "@tauri-apps/api/window";
import { logger } from "@/logger.js";
import { toggleMaximizeWindow } from "./windowControls.js";

/**
 * Title-bar dragging for the frameless window.
 *
 * The shared UI marks its title areas with `app-region: drag` and its buttons
 * with `app-region: no-drag` (Electron semantics). Tauri webviews ignore that
 * property, so a document-level mousedown handler starts the native drag (or
 * toggles maximize on double click) when the press lands in a drag region.
 */

type AppRegion = "drag" | "no-drag";

const DRAG_CLASS = "[app-region:drag]";
const NO_DRAG_CLASS = "[app-region:no-drag]";

// Interactive controls never start a drag, even inside a drag region.
const INTERACTIVE_SELECTOR = [
  "button",
  "a[href]",
  "input",
  "textarea",
  "select",
  "summary",
  "label",
  "[contenteditable]:not([contenteditable='false'])",
  "[role='button']",
  "[role='menuitem']",
  "[role='tab']",
  "[role='link']",
  "[role='textbox']",
  "[role='slider']",
  "[role='separator']",
].join(",");

function readComputedRegion(element: Element): AppRegion | null {
  const style = getComputedStyle(element);
  const value =
    style.getPropertyValue("app-region").trim() ||
    style.getPropertyValue("-webkit-app-region").trim();
  return value === "drag" || value === "no-drag" ? value : null;
}

function readClassRegion(element: Element): AppRegion | null {
  // WebKit-based webviews (macOS, Linux) drop the unknown property, so fall back to the
  // Tailwind arbitrary-property class the shared UI uses.
  if (element.classList.contains(NO_DRAG_CLASS)) {
    return "no-drag";
  }
  return element.classList.contains(DRAG_CLASS) ? "drag" : null;
}

/** The nearest element that declares a region decides, matching how the UI nests them. */
function isInDragRegion(target: Element): boolean {
  for (let element: Element | null = target; element !== null; element = element.parentElement) {
    const region = readComputedRegion(element) ?? readClassRegion(element);
    if (region !== null) {
      return region === "drag";
    }
  }
  return false;
}

function shouldDrag(event: MouseEvent): boolean {
  if (event.button !== 0 || event.defaultPrevented) {
    return false;
  }
  const target = event.target;
  if (!(target instanceof Element) || target.closest(INTERACTIVE_SELECTOR) !== null) {
    return false;
  }
  return isInDragRegion(target);
}

/**
 * Installs the drag handler once for the document. Returns its disposer.
 *
 * The listener runs in the bubble phase, so UI handlers that stop propagation
 * (resize handles, custom pointer interactions) keep their own behavior.
 */
export function installWindowDragRegions(target: Document = document): () => void {
  const onMouseDown = (event: MouseEvent): void => {
    if (!shouldDrag(event)) {
      return;
    }
    event.preventDefault();
    const action = event.detail === 2 ? toggleMaximizeWindow() : getCurrentWindow().startDragging();
    action.catch((error: unknown) => logger.warn("window drag action failed", { error }));
  };
  target.addEventListener("mousedown", onMouseDown);
  return () => target.removeEventListener("mousedown", onMouseDown);
}
