import { useMemo, useState, type FormEvent } from "react";
import { createRoot } from "react-dom/client";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  readAllowedRemoteOrigins,
  validateRemoteLink,
  type RemoteLinkRejection,
} from "./remoteLink.js";

/**
 * Mobile remote-client entry (Phase 4).
 *
 * The phone owns no Host, Agent, PTY, or file system. It pairs with a desktop
 * by opening the link shown as a QR code on the desktop; the webview then
 * loads the ZCode web remote page, which speaks the relay protocol with the
 * `web-remote-replayable` transport. Nothing here stores task or session state.
 */

interface MobileEnvironment {
  readonly VITE_HARNESS_REMOTE_ORIGINS?: string;
}

const environment = (import.meta as ImportMeta & { readonly env?: MobileEnvironment }).env;

const isChinese = (globalThis.navigator?.language ?? "").toLowerCase().startsWith("zh");

const TEXT = isChinese
  ? {
      title: "连接到桌面",
      body: "在桌面 Harness 中开启网页远程控制，然后粘贴二维码中的链接。手机只作为遥控端，不运行本地 Host 或 Agent。",
      label: "远程控制链接",
      placeholder: "https://zcode.z.ai/remote/v4?sid=…",
      open: "连接",
      rejections: {
        empty: "请输入链接。",
        invalid: "链接格式无效。",
        origin: "此链接的地址不在允许列表中。",
        insecure: "只允许 https 链接。",
        path: "这不是远程控制链接。",
        sid: "链接缺少设备标识（sid）。",
      } satisfies Record<RemoteLinkRejection, string>,
    }
  : {
      title: "Connect to your desktop",
      body: "Turn on web remote control in desktop Harness, then paste the link from its QR code. This phone is a remote client only; it runs no local Host or Agent.",
      label: "Remote control link",
      placeholder: "https://zcode.z.ai/remote/v4?sid=…",
      open: "Connect",
      rejections: {
        empty: "Enter a link.",
        invalid: "The link is not a valid URL.",
        origin: "This link's origin is not on the allowlist.",
        insecure: "Only https links are allowed.",
        path: "This is not a remote control link.",
        sid: "The link has no device id (sid).",
      } satisfies Record<RemoteLinkRejection, string>,
    };

function RemotePairing(): React.JSX.Element {
  const allowedOrigins = useMemo(
    () => readAllowedRemoteOrigins(environment?.VITE_HARNESS_REMOTE_ORIGINS),
    [],
  );
  const [link, setLink] = useState("");
  const [rejection, setRejection] = useState<RemoteLinkRejection | null>(null);

  const submit = (event: FormEvent<HTMLFormElement>): void => {
    event.preventDefault();
    const result = validateRemoteLink(link, allowedOrigins);
    if (!result.ok) {
      setRejection(result.reason);
      return;
    }
    setRejection(null);
    // 顶层导航到同一份 ZCode 网页远控页面；该页面不获得任何 Tauri IPC 能力。
    window.location.assign(result.url);
  };

  return (
    <main
      data-testid="harness-mobile-pairing"
      className="bg-background text-foreground flex min-h-dvh flex-col justify-center gap-6 px-4 py-8"
    >
      <header className="flex flex-col gap-2">
        <h1 className="text-xl font-semibold">{TEXT.title}</h1>
        <p className="text-foreground-subtle text-sm leading-relaxed">{TEXT.body}</p>
      </header>
      <form className="flex flex-col gap-3" onSubmit={submit} noValidate>
        <label className="text-sm font-medium" htmlFor="harness-remote-link">
          {TEXT.label}
        </label>
        <Input
          id="harness-remote-link"
          type="url"
          inputMode="url"
          autoComplete="off"
          autoCapitalize="off"
          spellCheck={false}
          placeholder={TEXT.placeholder}
          value={link}
          onChange={(event) => setLink(event.target.value)}
          aria-invalid={rejection !== null}
          aria-describedby={rejection ? "harness-remote-link-error" : undefined}
        />
        {rejection ? (
          <p id="harness-remote-link-error" role="alert" className="text-destructive text-sm">
            {TEXT.rejections[rejection]}
          </p>
        ) : null}
        <Button type="submit">{TEXT.open}</Button>
      </form>
    </main>
  );
}

/** Renders the pairing screen into `#root`. */
export function startHarnessMobile(): void {
  const container = document.getElementById("root");
  if (container === null) {
    throw new Error("Harness root element is missing");
  }
  createRoot(container).render(<RemotePairing />);
}
