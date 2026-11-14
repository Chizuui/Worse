#!/usr/bin/env node
// Signs one file with the configured Windows Authenticode credentials and verifies the result.
// Used by build-installer.mjs as Tauri's bundle.windows.signCommand (harness-app.exe, the
// uninstaller and the installer) and by sign-staged-resources.mjs for host/ PE files.
// Spec: Worse/docs/specs/harness/phase-6-packaging.md section 5.2. Secrets are never printed.
import { readdir } from "node:fs/promises";
import path from "node:path";
import process from "node:process";
import { pathToFileURL } from "node:url";
import { fail, isFile, run } from "./lib.mjs";
import { signingDecision } from "./signing-policy.mjs";

const DEFAULT_TIMESTAMP = "http://timestamp.digicert.com";

function powershell() {
  // Store 版 pwsh.exe 在门禁宿主里不可用，固定使用系统自带的 Windows PowerShell。
  return path.join(process.env.SystemRoot ?? "C:\\Windows", "System32", "WindowsPowerShell", "v1.0", "powershell.exe");
}

/** Newest signtool.exe from the Windows SDK, or HARNESS_SIGNTOOL. */
export async function findSigntool() {
  const explicit = process.env.HARNESS_SIGNTOOL?.trim();
  if (explicit) return (await isFile(explicit)) ? explicit : fail(`HARNESS_SIGNTOOL=${explicit} does not exist`);
  const kits = path.join(process.env["ProgramFiles(x86)"] ?? "C:\\Program Files (x86)", "Windows Kits", "10", "bin");
  const arch = process.arch === "arm64" ? "arm64" : "x64";
  const versions = (await readdir(kits).catch(() => []))
    .filter((name) => /^10\.\d+\.\d+\.\d+$/.test(name))
    .sort((a, b) => b.localeCompare(a, undefined, { numeric: true }));
  for (const version of versions) {
    const candidate = path.join(kits, version, arch, "signtool.exe");
    if (await isFile(candidate)) return candidate;
  }
  return fail("signtool.exe not found; install the Windows SDK or set HARNESS_SIGNTOOL");
}

/**
 * Authenticode status for each file: Map<file, { status, signer, timestamped }>. One PowerShell
 * process for the whole list.
 */
export async function authenticodeStatus(files) {
  const result = new Map();
  if (files.length === 0) return result;
  const script =
    "$ErrorActionPreference='Stop'; $files = [Console]::In.ReadToEnd() | ConvertFrom-Json; " +
    "@($files | ForEach-Object { $s = Get-AuthenticodeSignature -LiteralPath $_; " +
    "[pscustomobject]@{ file = $_; status = [string]$s.Status; " +
    "signer = $(if ($s.SignerCertificate) { $s.SignerCertificate.Subject } else { $null }); " +
    "timestamped = [bool]$s.TimeStamperCertificate } }) | ConvertTo-Json -Compress -Depth 3";
  // 修复原因：从 PowerShell 7 启动时继承的 PSModulePath 指向 pwsh 7 的模块，Windows PowerShell 5.1
  // 会因此加载 Microsoft.PowerShell.Security 失败；去掉它让 5.1 使用自身默认模块路径。
  const env = { ...process.env };
  delete env.PSModulePath;
  const probe = await run(powershell(), ["-NoProfile", "-NonInteractive", "-Command", script], {
    input: JSON.stringify(files),
    env,
  });
  if (probe.code !== 0) fail(`Get-AuthenticodeSignature failed: ${probe.stderr.trim()}`);
  const parsed = JSON.parse(probe.stdout.trim() || "[]");
  for (const entry of Array.isArray(parsed) ? parsed : [parsed]) result.set(entry.file, entry);
  return result;
}

/** Valid, or (test only, HARNESS_SIGN_ALLOW_UNTRUSTED=1) signed by an untrusted root. */
export function acceptable(entry) {
  if (entry?.status === "Valid") return true;
  const untrustedOk = process.env.HARNESS_SIGN_ALLOW_UNTRUSTED?.trim() === "1";
  return untrustedOk && Boolean(entry?.signer) && ["UnknownError", "NotTrusted"].includes(entry.status);
}

async function signWithCredentials(file, credentials) {
  if (credentials.kind === "command") {
    const command = credentials.command.replaceAll("{file}", `"${file}"`);
    return run(command, [], { shell: true });
  }
  const signtool = await findSigntool();
  const timestamp = process.env.HARNESS_WINDOWS_TIMESTAMP_URL?.trim() || DEFAULT_TIMESTAMP;
  const identity =
    credentials.kind === "thumbprint"
      ? ["/sha1", credentials.thumbprint]
      : ["/f", credentials.file, ...(credentials.password ? ["/p", credentials.password] : [])];
  return run(signtool, ["sign", "/fd", "SHA256", "/tr", timestamp, "/td", "SHA256", ...identity, file]);
}

/** Signs `file` and verifies it; exits non-zero on any failure. */
export async function signWindowsFile(file, credentials) {
  if (!(await isFile(file))) fail(`cannot sign missing file ${file}`);
  const signed = await signWithCredentials(file, credentials);
  // 签名工具输出可能包含证书路径等环境信息，但不会包含密码；失败时只打印工具的 stderr/stdout 摘要。
  if (signed.code !== 0) fail(`signing ${file} failed (exit ${signed.code}): ${(signed.stderr || signed.stdout).trim().slice(0, 2000)}`);
  const status = (await authenticodeStatus([file])).get(file);
  if (!acceptable(status)) fail(`signature of ${file} did not verify: ${status?.status ?? "no result"}`);
  if (!status.timestamped && process.env.HARNESS_SIGN_ALLOW_UNTRUSTED?.trim() !== "1") {
    fail(`signature of ${file} carries no RFC 3161 timestamp`);
  }
  process.stdout.write(`signed ${path.basename(file)} (${status.status}${status.timestamped ? ", timestamped" : ""})\n`);
}

async function main() {
  const file = process.argv[2];
  if (!file || file === "--help" || file === "-h") {
    process.stdout.write("usage: node Worse/scripts/package/sign-file.mjs <file>\n");
    return;
  }
  if (process.platform !== "win32") fail("sign-file.mjs signs Windows PE files only");
  const decision = signingDecision("win32");
  if (decision.action === "fail") fail(decision.reason);
  if (decision.action === "skip") {
    process.stdout.write(`not signing ${path.basename(file)}: ${decision.reason}\n`);
    return;
  }
  await signWindowsFile(path.resolve(file), decision.credentials);
}

if (process.argv[1] && import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href) {
  await main();
}
