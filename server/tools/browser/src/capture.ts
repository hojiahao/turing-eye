import { constants } from "node:fs";
import { lstat, open, readdir, realpath } from "node:fs/promises";
import { createHash, randomUUID } from "node:crypto";
import { join } from "node:path";
import type { Page } from "playwright-core";
import type { Artifact } from "./protocol.js";

export function redact(text: string, secrets: readonly string[], maximum = 2048): string {
  let result = text;
  for (const secret of secrets) if (secret.length) result = result.replaceAll(secret, "[redacted]");
  result = result.replace(/https?:\/\/[^\s<>"']+/g, (value) => {
    try { const url = new URL(value); return `${url.origin}/[redacted]`; } catch { return "[redacted_url]"; }
  });
  return [...result.replace(/[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f]/g, "")].slice(0, maximum).join("");
}

export function redactedUrl(raw: string): string {
  const url = new URL(raw);
  // The R1 fixture has a closed route set. Other paths must not become log content.
  const path = ["/", "/game", "/slow", "/missing", "/state"].includes(url.pathname) ? url.pathname : "/redacted";
  return `${url.origin}${path}`;
}

export async function describePage(page: Page, secrets: readonly string[]): Promise<Record<string, unknown>> {
  return { url: redactedUrl(page.url()), title: redact(await page.title(), secrets, 512) };
}

export async function captureDom(page: Page, secrets: readonly string[], maximum: number): Promise<{ text: string; truncated: boolean }> {
  const captured = await page.evaluate((limit) => {
    const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
    const parts: string[] = [];
    let length = 0;
    let truncated = false;
    while (walker.nextNode()) {
      const parent = walker.currentNode.parentElement;
      if (!parent || parent.closest("script,style,noscript,template,input,textarea,[hidden],[data-private],[aria-hidden='true']")) continue;
      const style = getComputedStyle(parent);
      if (!parent.getClientRects().length || style.visibility === "hidden" || style.display === "none") continue;
      const text = walker.currentNode.textContent?.trim() ?? "";
      const remaining = limit - length;
      const characters = [...text];
      parts.push(characters.slice(0, Math.max(0, remaining)).join(""));
      length += characters.length + 1;
      if (length > limit) { truncated = true; break; }
    }
    return { text: parts.filter(Boolean).join("\n"), truncated };
  }, maximum);
  return { text: redact(captured.text, secrets, maximum), truncated: captured.truncated };
}

export async function stageScreenshot(spoolRoot: string, sessionId: string, bytes: Buffer): Promise<Artifact> {
  if (!bytes.length || bytes.length > 8_388_608) throw new Error("artifact_size");
  const folder = join(spoolRoot, sessionId);
  const stat = await lstat(folder);
  if (!stat.isDirectory() || stat.isSymbolicLink() || await realpath(folder) !== folder) throw new Error("spool_directory");
  const names = await readdir(folder);
  if (names.length >= 1024) throw new Error("spool_file_limit");
  let size = bytes.length;
  for (const name of names) {
    const entry = await lstat(join(folder, name));
    if (!entry.isFile() || entry.isSymbolicLink()) throw new Error("spool_file_type");
    size += entry.size;
  }
  if (size > 67_108_864) throw new Error("spool_byte_limit");
  const handle = `capture_${randomUUID()}`;
  const file = await open(join(folder, handle), constants.O_WRONLY | constants.O_CREAT | constants.O_EXCL | constants.O_NOFOLLOW, 0o600);
  try { await file.writeFile(bytes); await file.sync(); await file.chmod(0o400); }
  finally { await file.close(); }
  return { handle, media_type: "image/png", byte_length: bytes.length, sha256: createHash("sha256").update(bytes).digest("hex") };
}
