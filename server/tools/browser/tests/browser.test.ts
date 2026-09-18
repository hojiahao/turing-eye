import { test } from "node:test";
import assert from "node:assert/strict";
import { createServer, type ServerResponse } from "node:http";
import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { access, mkdir, mkdtemp, readFile, readdir, rm, stat, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { createHash } from "node:crypto";
import { once } from "node:events";
import { PNG } from "pngjs";
import { packageRoot, assertResponseMatches, validateFrame, type Owner, type Request, type Response, type Tool, type ToolParameters } from "../src/protocol.js";

const knownSecrets = ["R1_SECRET_SENTINEL", "R1_PASSWORD_SENTINEL"];
const owner: Owner = { tenant_id: "tenant_r1", run_id: "run_a", task_id: "task_play", attempt_id: "attempt_a", attempt_generation: 1 };
const ownerB = { ...owner, run_id: "run_b", attempt_id: "attempt_b" };
let nextId = 0;

function request<K extends Tool>(sessionId: string, tool: K, params: ToolParameters[K], asOwner = owner, budget = 15_000): Request {
  const id = ++nextId;
  return { protocol_version: "2.0", type: "request", request_id: `req_${id}`, call_id: `call_${id}`, owner: asOwner,
    session_id: sessionId, tool, params, deadline_at: new Date(Date.now() + budget).toISOString(), max_result_bytes: 65_536 } as Request;
}
function data(response: Response): Record<string, unknown> {
  assert.equal(response.type, "result", JSON.stringify(response));
  if (response.type !== "result") throw new Error("expected_result");
  return response.data;
}
function failure(response: Response, code: string): void {
  assert.equal(response.type, "error", JSON.stringify(response));
  if (response.type === "error") assert.equal(response.error.code, code);
}
function domText(response: Response): string { return (data(response).dom as { text: string }).text; }

class PipeClient {
  readonly frames: Response[] = [];
  readonly child: ChildProcessWithoutNullStreams;
  readonly exited: Promise<{ code: number | null; signal: NodeJS.Signals | null }>;
  readonly pending = new Map<string, { request: Request; resolve: (frame: Response) => void; reject: (error: Error) => void; timer: NodeJS.Timeout }>();
  stderr = "";
  private buffer = "";
  constructor(configPath: string, temporaryDirectory: string) {
    this.child = spawn(process.execPath, [join(packageRoot, "dist/src/main.js")], {
      cwd: packageRoot, env: { PATH: process.env.PATH ?? "/usr/bin:/bin", TMPDIR: temporaryDirectory, R1_BROWSER_CONFIG: configPath }, stdio: "pipe",
    });
    this.child.stdout.setEncoding("utf8");
    this.child.stdin.on("error", () => undefined);
    this.child.stderr.setEncoding("utf8");
    this.child.stderr.on("data", (chunk: string) => { this.stderr = (this.stderr + chunk).slice(-8192); });
    this.child.stdout.on("data", (chunk: string) => {
      this.buffer += chunk;
      let newline: number;
      while ((newline = this.buffer.indexOf("\n")) >= 0) {
        const line = this.buffer.slice(0, newline);
        this.buffer = this.buffer.slice(newline + 1);
        const frame = JSON.parse(line) as Response;
        assert.equal(validateFrame(frame), true, JSON.stringify(validateFrame.errors));
        const waiter = this.pending.get(frame.request_id);
        assert.ok(waiter, "No duplicate or uncorrelated response is allowed");
        assert.ok(Buffer.byteLength(line) + 1 <= waiter.request.max_result_bytes);
        assertResponseMatches(waiter.request, frame);
        clearTimeout(waiter.timer);
        this.pending.delete(frame.request_id);
        this.frames.push(frame);
        waiter.resolve(frame);
      }
      assert.ok(Buffer.byteLength(this.buffer) <= 262_144);
    });
    this.exited = new Promise((resolve) => {
      this.child.on("error", (error) => { for (const waiter of this.pending.values()) waiter.reject(error); });
      this.child.on("close", (code, signal) => {
        for (const waiter of this.pending.values()) { clearTimeout(waiter.timer); waiter.reject(new Error("tool_process_exited_without_response")); }
        this.pending.clear();
        resolve({ code, signal });
      });
    });
  }
  send(frame: Request): Promise<Response> {
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => { this.pending.delete(frame.request_id); reject(new Error(`response_timeout:${frame.request_id}`)); }, 20_000);
      this.pending.set(frame.request_id, { request: frame, resolve, reject, timer });
      this.child.stdin.write(`${JSON.stringify(frame)}\n`);
    });
  }
  async stop(): Promise<void> {
    if (this.child.exitCode !== null || this.child.signalCode !== null) return;
    this.child.stdin.end();
    const timer = setTimeout(() => this.child.kill("SIGKILL"), 10_000);
    try { assert.deepEqual(await this.exited, { code: 0, signal: null }, this.stderr); }
    finally { clearTimeout(timer); }
  }
}

async function chromiumPath(): Promise<string> {
  if (process.env.CHROMIUM_PATH) { await access(process.env.CHROMIUM_PATH); return process.env.CHROMIUM_PATH; }
  const root = "/root/.cache/ms-playwright";
  const installed = (await readdir(root)).filter((entry) => /^chromium-\d+$/.test(entry)).sort().reverse();
  for (const directory of installed) {
    const candidate = join(root, directory, "chrome-linux64/chrome");
    try { await access(candidate); return candidate; } catch { /* Try the next existing build. */ }
  }
  throw new Error("No existing Chromium found; this experiment never downloads one.");
}

async function descendants(parent: number): Promise<{ pid: number; state: string; command: string }[]> {
  const records: { pid: number; parent: number; state: string; command: string }[] = [];
  for (const name of await readdir("/proc")) {
    if (!/^\d+$/.test(name)) continue;
    try {
      const text = await readFile(`/proc/${name}/stat`, "utf8");
      const fields = text.slice(text.lastIndexOf(")") + 2).split(" ");
      const command = (await readFile(`/proc/${name}/cmdline`, "utf8")).replaceAll("\0", " ");
      records.push({ pid: Number(name), parent: Number(fields[1]), state: fields[0]!, command });
    } catch { /* A process may exit while /proc is sampled. */ }
  }
  const ids = new Set([parent]);
  for (let index = 0; index < 20; index++) {
    const before = ids.size;
    for (const record of records) if (ids.has(record.parent)) ids.add(record.pid);
    if (ids.size === before) break;
  }
  return records.filter((record) => record.pid !== parent && ids.has(record.pid));
}

async function assertExited(pids: number[]): Promise<void> {
  for (const pid of pids) {
    try {
      const text = await readFile(`/proc/${pid}/stat`, "utf8");
      assert.fail(`Descendant ${pid} remains after cleanup: ${text.slice(text.lastIndexOf(")") + 2, text.lastIndexOf(")") + 3)}`);
    } catch (error) {
      if (error instanceof Error && "code" in error && error.code === "ENOENT") continue;
      throw error;
    }
  }
}

test("R1 real JSONL/browser lifecycle on a fixed local Canvas fixture", { timeout: 90_000 }, async (t) => {
  await mkdir(join(packageRoot, ".artifacts"), { recursive: true });
  const directory = await mkdtemp(join(packageRoot, ".artifacts/r1-"));
  const spoolRoot = join(directory, "spool");
  // Chromium's Unix socket path must fit Linux's sockaddr_un limit.
  const temporaryDirectory = await mkdtemp(join(packageRoot, ".t-"));
  await mkdir(spoolRoot, { mode: 0o700 });
  const fixture = await readFile(join(packageRoot, "fixtures/game.html"));
  const pendingResponses = new Set<ServerResponse>();
  const slowWaiters: (() => void)[] = [];
  const server = createServer((req, res) => {
    if (req.url === "/slow") {
      pendingResponses.add(res);
      res.on("close", () => pendingResponses.delete(res));
      for (const waiter of slowWaiters.splice(0)) waiter();
      return;
    }
    if (req.url === "/missing") { res.writeHead(404); res.end("Fixture not found"); return; }
    res.writeHead(200, { "content-type": "text/html; charset=utf-8", "cache-control": "no-store" });
    res.end(fixture);
  });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  const address = server.address();
  assert.ok(address && typeof address !== "string");
  const fixtureOrigin = `http://127.0.0.1:${address.port}`;
  const executable = await chromiumPath();
  const configPath = join(directory, "config.json");
  await writeFile(configPath, JSON.stringify({ chromiumPath: executable, spoolRoot, fixtureOrigin, fixtureOwners: [owner, ownerB], knownSecrets }), { mode: 0o600 });
  const client = new PipeClient(configPath, temporaryDirectory);
  t.after(async () => {
    try { await client.stop(); }
    finally {
      for (const response of pendingResponses) response.destroy();
      server.closeAllConnections();
      await new Promise<void>((resolve) => server.close(() => resolve()));
      await rm(temporaryDirectory, { recursive: true, force: true });
    }
  });
  async function step(name: string, check: () => Promise<void>): Promise<void> {
    let succeeded = false;
    await t.test(name, async () => { await check(); succeeded = true; });
    assert.ok(succeeded, `Dependent browser steps stopped after: ${name}`);
  }
  async function create(session: string, asOwner = owner, path = "/game", lifetime = 60_000, budget = 15_000): Promise<{ frame: Request; response: Promise<Response> }> {
    await mkdir(join(spoolRoot, session), { mode: 0o700 });
    const frame = request(session, "open", { mode: "create", url: `${fixtureOrigin}${path}`,
      policy: { revision: "policy_r1", navigation_origins: [fixtureOrigin], allow_public_subresources: false, private_target_ids: ["r1_fixture"] },
      viewport: { width_px: 640, height_px: 480 }, expires_at: new Date(Date.now() + lifetime).toISOString() }, asOwner, budget);
    return { frame, response: client.send(frame) };
  }
  const observe = (session: string, asOwner = owner, screenshot = false) => client.send(request(session, "observe", { include_dom: true, include_screenshot: screenshot }, asOwner));
  const close = (session: string, asOwner = owner) => client.send(request(session, "close", {}, asOwner, 4000));
  const captured: string[] = [];
  let processIds: number[] = [];

  await step("open, real mouse/keyboard, shared Chromium and isolated Context state", async () => {
    data(await (await create("session_a")).response);
    const initial = await observe("session_a", owner, true);
    assert.match(domText(initial), /Position 0\. Cookie fresh/);
    data(await client.send(request("session_a", "mouse", { action: "click", x_px: 70, y_px: 115, button: "left", click_count: 1 })));
    data(await client.send(request("session_a", "keyboard", { action: "press", key: "ArrowRight" })));
    const advanced = await observe("session_a", owner, true);
    assert.match(domText(advanced), /Position [12]\. Cookie present/);
    data(await (await create("session_b", ownerB)).response);
    assert.match(domText(await observe("session_b", ownerB)), /Position 0\. Cookie fresh/);
    data(await client.send(request("session_a", "open", { mode: "navigate", url: `${fixtureOrigin}/game` })));
    assert.match(domText(await observe("session_a")), /Cookie present/);
    const processes = await descendants(client.child.pid!);
    processIds = processes.map((entry) => entry.pid);
    assert.equal(processes.filter((entry) => entry.command.includes(executable) && !entry.command.includes("--type=")).length, 1, "Both sessions must share one Chromium root process");
    const pngs: PNG[] = [];
    for (const frame of [initial, advanced]) {
      assert.equal(frame.artifacts.length, 1);
      const artifact = frame.artifacts[0]!;
      const path = join(spoolRoot, "session_a", artifact.handle);
      captured.push(path);
      const bytes = await readFile(path);
      assert.equal(bytes.length, artifact.byte_length);
      assert.equal(createHash("sha256").update(bytes).digest("hex"), artifact.sha256);
      assert.equal((await stat(path)).mode & 0o777, 0o400);
      const png = PNG.sync.read(bytes);
      assert.deepEqual([png.width, png.height], [640, 480]);
      const maskPixel = (355 * png.width + 25) * 4;
      assert.deepEqual([...png.data.subarray(maskPixel, maskPixel + 4)], [0, 0, 0, 255]);
      const colors = new Set<string>();
      for (let i = 0; i < png.data.length; i += 4) colors.add(png.data.subarray(i, i + 3).toString("hex"));
      assert.ok(colors.has("207f62") && colors.has("c84d48") && colors.has("dcecf0"), "Canvas pixels must contain the rendered game objects");
      assert.ok(!JSON.stringify(frame).includes("R1_SECRET"));
      pngs.push(png);
    }
    assert.notDeepEqual(pngs[0]!.data, pngs[1]!.data, "Actual input must change the rendered page");
  });

  await step("read console/network metadata, redaction and stream cursors", async () => {
    const consoleFrame = await client.send(request("session_a", "console", { after_sequence: 0, limit: 20 }));
    assert.match(JSON.stringify(data(consoleFrame)), /Fixture ready/);
    assert.ok(!JSON.stringify(consoleFrame).includes(knownSecrets[0]!));
    const networkFrame = await client.send(request("session_a", "network", { after_sequence: 0, limit: 20 }));
    assert.match(JSON.stringify(data(networkFrame)), /"status_code":404/);
    failure(await client.send(request("session_a", "network", { after_sequence: 99999, limit: 20 })), "INVALID_ARGUMENT");
    const page = await client.send({ ...request("session_a", "console", { after_sequence: 0, limit: 1 }), max_result_bytes: 8192 });
    assert.equal((data(page).entries as unknown[]).length, 1);
  });

  await step("validate owner, generation, bounds and schema before tool side effects", async () => {
    for (const changed of [{ ...owner, tenant_id: "other" }, { ...owner, run_id: "other" }, { ...owner, task_id: "other" },
      { ...owner, attempt_id: "other" }, { ...owner, attempt_generation: 2 }]) {
      const frame = await observe("session_a", changed);
      failure(frame, "SESSION_UNAVAILABLE");
      if (frame.type === "error") assert.equal(frame.error.context_state, "unknown");
    }
    failure(await client.send(request("session_a", "mouse", { action: "move", x_px: 1000, y_px: 300 })), "INVALID_ARGUMENT");
    failure(await client.send(request("session_a", "observe", { include_dom: false, include_screenshot: false })), "INVALID_ARGUMENT");
    failure(await client.send(request("session_a", "open", { mode: "navigate", url: "https://example.com/" })), "POLICY_DENIED");
    // A validation failure must not close the existing useful session.
    assert.match(domText(await observe("session_a")), /Cookie present/);
    assert.deepEqual(data(await close("session_a")), { context_state: "closed" });
    assert.deepEqual(data(await close("session_a")), { context_state: "closed" });
    assert.match(domText(await observe("session_b", ownerB)), /Position 0/);
    failure(await observe("session_a"), "SESSION_CLOSING");
  });

  await step("control path cancels a real hung navigation without blocking behind it", async () => {
    const arrived = new Promise<void>((resolve) => slowWaiters.push(resolve));
    const opening = await create("session_cancel", owner, "/slow");
    await Promise.race([arrived, opening.response.then((frame) => { data(frame); assert.fail("The slow fixture must not complete before cancellation"); })]);
    failure(await observe("session_cancel"), "CAPACITY_EXCEEDED");
    failure(await client.send(request("session_cancel", "cancel", { target_request_id: opening.frame.request_id,
      target_call_id: "wrong", reason: "run_cancelled" }, owner, 4000)), "CANCEL_TARGET_INVALID");
    const cancellation = await client.send(request("session_cancel", "cancel", { target_request_id: opening.frame.request_id,
      target_call_id: opening.frame.call_id, reason: "run_cancelled" }, owner, 4000));
    assert.equal(data(cancellation).disposition, "accepted");
    assert.ok(["closing", "closed"].includes(String(data(cancellation).context_state)));
    const cancelled = await opening.response;
    failure(cancelled, "CANCELLED");
    if (cancelled.type === "error") {
      assert.equal(cancelled.error.external_effects, "possible");
      if (cancelled.error.context_state !== "closed") assert.equal(cancelled.error.local_execution, "unknown");
      if (cancelled.error.local_execution === "stopped") assert.equal(cancelled.error.context_state, "closed");
    }
    assert.deepEqual(data(await close("session_cancel")), { context_state: "closed" });
    const again = await client.send(request("session_cancel", "cancel", { target_request_id: opening.frame.request_id,
      target_call_id: opening.frame.call_id, reason: "run_cancelled" }, owner, 4000));
    assert.equal(data(again).disposition, "already_terminal");
    assert.equal(client.frames.filter((frame) => frame.request_id === opening.frame.request_id).length, 1);
    assert.match(domText(await observe("session_b", ownerB)), /Position 0/);
  });

  await step("deadline cleans a hanging operation; closed sessions reject further input", async () => {
    const opening = await create("session_deadline", owner, "/slow", 3000, 400);
    failure(await opening.response, "DEADLINE_EXCEEDED");
    assert.deepEqual(data(await close("session_deadline")), { context_state: "closed" });
    failure(await client.send(request("session_deadline", "keyboard", { action: "press", key: "Space" }, owner, 500)), "SESSION_CLOSING");
  });

  await step("EOF releases active Contexts and the shared Chromium process tree", async () => {
    const remainingProcesses = await descendants(client.child.pid!);
    processIds = [...new Set([...processIds, ...remainingProcesses.map((entry) => entry.pid)])];
    await client.stop();
    assert.equal(client.pending.size, 0);
    await assertExited(processIds);
    assert.ok(!(await readdir(temporaryDirectory)).some((name) => name.startsWith("playwright_")), "Playwright profiles must be removed on close");
    await rm(temporaryDirectory, { recursive: true, force: true });
    for (const path of captured) await access(path); // Closing must not delete unconsumed evidence.
    const tools = [...new Set(client.frames.map((frame) => frame.tool))].sort();
    assert.equal(tools.length, 8);
    await writeFile(join(directory, "result.json"), JSON.stringify({ status: "passed", fixture: "game.html", transport: "stdin/stdout JSONL 2.0",
      chromium: executable, shared_chromium_roots: 1, tools, response_count: client.frames.length,
      descendant_pids_reaped: processIds, captures: captured, temporary_directory_empty: true,
      scope: "R1 local fixture experiment only; no R5 egress/security or Rust host acceptance claim" }, null, 2));
  });
});

test("transport faults close the channel without invented correlated replies", { timeout: 40_000 }, async (t) => {
  const directory = await mkdtemp(join(packageRoot, ".artifacts/framing-"));
  await mkdir(join(directory, "temporary"));
  const configPath = join(directory, "config.json");
  await writeFile(configPath, JSON.stringify({ chromiumPath: await chromiumPath(), spoolRoot: directory, fixtureOrigin: "http://127.0.0.1:1",
    fixtureOwners: [owner], knownSecrets: [] }));
  const unknown = request("unused", "close", {}, owner, 4000);
  for (const bytes of [Buffer.from('\n'), Buffer.from('{"a":1,"a":2}\n'), Buffer.from('{"unfinished":'), Buffer.from([0xff, 10]),
    Buffer.alloc(65_537, 120), Buffer.from(`${JSON.stringify({ ...unknown, protocol_version: "invalid" })}\n`)]) {
    const client = new PipeClient(configPath, join(directory, "temporary"));
    t.after(() => client.stop());
    client.child.stdin.end(bytes);
    const timer = setTimeout(() => client.child.kill("SIGKILL"), 8000);
    const outcome = await client.exited.finally(() => clearTimeout(timer));
    assert.equal(outcome.code, 2);
    assert.equal(client.frames.length, 0);
    assert.match(client.stderr, /^r1_browser:/);
  }
  const duplicateClient = new PipeClient(configPath, join(directory, "temporary"));
  t.after(() => duplicateClient.stop());
  const repeated = request("unused", "close", {}, owner, 4000);
  failure(await duplicateClient.send(repeated), "SESSION_UNAVAILABLE");
  duplicateClient.child.stdin.end(`${JSON.stringify(repeated)}\n`);
  const timer = setTimeout(() => duplicateClient.child.kill("SIGKILL"), 8000);
  assert.equal((await duplicateClient.exited.finally(() => clearTimeout(timer))).code, 2);
  assert.equal(duplicateClient.frames.length, 1);
});
