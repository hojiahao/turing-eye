import { chromium, type Browser, type BrowserContext, type Page } from "playwright-core";
import { performance } from "node:perf_hooks";
import { realpath, stat } from "node:fs/promises";
import {
  assertResponseMatches, ProtocolFault, sameOwner, validateEnvelope, validateRequest,
  type Artifact, type ContextState, type Failure, type Owner, type Request, type Response,
} from "./protocol.js";
import { captureDom, describePage, redact, redactedUrl, stageScreenshot } from "./capture.js";

export type ExperimentConfig = {
  chromiumPath: string; spoolRoot: string; fixtureOrigin: string;
  fixtureOwners: { tenant_id: string; run_id: string }[]; knownSecrets: string[];
};
type Entry = Record<string, unknown> & { sequence: number };
class ObservationLog {
  sequence = 0;
  entries: Entry[] = [];
  append(value: Record<string, unknown>): void {
    this.entries.push({ sequence: ++this.sequence, observed_at: new Date().toISOString(), ...value });
    if (this.entries.length > 128) this.entries.shift();
  }
  read(after: number, limit: number): Record<string, unknown> {
    if (after > this.sequence) throw new OperationFailure("INVALID_ARGUMENT", "read", "The cursor is beyond this stream's watermark.");
    const entries = this.entries.filter((entry) => entry.sequence > after).slice(0, limit);
    const next = entries.at(-1)?.sequence ?? after;
    return { entries, next_after_sequence: next, has_more: next < this.sequence, gap: (this.entries[0]?.sequence ?? 1) > after + 1 };
  }
}
type Call = {
  request: Request; startedAt: number; expiresAt: number; abort: AbortController;
  terminal: boolean; actionStarted: boolean;
};
type Session = {
  id: string; owner: Owner; state: "creating" | "open" | "closing" | "closed";
  expiresAt: number; expiryTimer?: NodeJS.Timeout; context?: BrowserContext; page?: Page;
  allocation?: Promise<void>; closing?: Promise<void>; active?: Call;
  console: ObservationLog; network: ObservationLog;
};
class OperationFailure extends Error {
  constructor(public readonly code: string, public readonly stage: Failure["stage"], detail: string) { super(detail); }
}

export async function validateConfig(config: ExperimentConfig): Promise<void> {
  const origin = new URL(config.fixtureOrigin);
  if (origin.origin !== config.fixtureOrigin || origin.protocol !== "http:" || origin.hostname !== "127.0.0.1" || !origin.port
    || !Array.isArray(config.fixtureOwners) || !config.fixtureOwners.length || config.fixtureOwners.length > 8
    || config.fixtureOwners.some((owner) => !/^[A-Za-z0-9_-]{1,64}$/.test(owner.tenant_id) || !/^[A-Za-z0-9_-]{1,64}$/.test(owner.run_id))
    || !Array.isArray(config.knownSecrets) || config.knownSecrets.length > 32
    || config.knownSecrets.some((secret) => typeof secret !== "string" || secret.length > 4096)) throw new Error("invalid_r1_config");
  if (await realpath(config.spoolRoot) !== config.spoolRoot || !(await stat(config.spoolRoot)).isDirectory()
    || !(await stat(config.chromiumPath)).isFile()) throw new Error("invalid_r1_paths");
}

export class BrowserWorker {
  private browserPromise?: Promise<Browser>;
  private browser?: Browser;
  private readonly sessions = new Map<string, Session>();
  private readonly calls = new Map<string, Call>();
  private readonly callIds = new Set<string>();
  private readonly tasks = new Set<Promise<void>>();
  private stopping = false;
  private inFlight = 0;
  private shutdownPromise?: Promise<void>;

  constructor(private readonly config: ExperimentConfig, private readonly output: (frame: Response) => Promise<void>) {}

  accept(raw: unknown): void {
    if (this.stopping) throw new ProtocolFault("channel_closing");
    if (!validateEnvelope(raw)) throw new ProtocolFault("invalid_envelope");
    const request = raw as Request;
    if (this.calls.has(request.request_id) || this.callIds.has(request.call_id)) throw new ProtocolFault("duplicate_request");
    // Retain correlation tombstones rather than allowing an old click to execute again.
    if (this.calls.size >= 1024 || this.inFlight >= 8) throw new ProtocolFault("channel_capacity");
    const startedAt = performance.now();
    const remaining = Date.parse(request.deadline_at) - Date.now();
    const call: Call = { request, startedAt, expiresAt: startedAt + remaining, abort: new AbortController(), terminal: false, actionStarted: false };
    this.calls.set(request.request_id, call);
    this.callIds.add(request.call_id);
    this.inFlight++;
    const task = this.execute(call, validateRequest(raw), remaining).finally(() => { this.inFlight--; this.tasks.delete(task); });
    this.tasks.add(task);
    // A broken output pipe must reach the transport owner, not become an unhandled rejection.
    void task.catch(() => { void this.shutdown().catch(() => undefined); });
  }

  private base(call: Call): Omit<Response, "type" | "data" | "error"> {
    const r = call.request;
    return { protocol_version: "2.0", request_id: r.request_id, call_id: r.call_id, owner: r.owner,
      session_id: r.session_id, tool: r.tool, completed_at: new Date().toISOString(),
      duration_ms: Math.max(0, Math.floor(performance.now() - call.startedAt)), artifacts: [], facts: [] };
  }

  private contextState(session: Session | undefined): ContextState {
    return session?.state === "creating" ? "unknown" : session?.state ?? "unknown";
  }

  private error(call: Call, error: OperationFailure, session?: Session): Extract<Response, { type: "error" }> {
    const stopped = session?.state === "closed";
    const retryable = ["CAPACITY_EXCEEDED", "BROWSER_DISCONNECTED", "NAVIGATION_FAILED", "OBSERVATION_FAILED", "ARTIFACT_FAILED"].includes(error.code);
    return { ...this.base(call), type: "error", error: { code: error.code, stage: error.stage, detail: error.message, retryable,
      local_execution: !call.actionStarted ? "not_started" : stopped ? "stopped" : "unknown",
      context_state: this.contextState(session), external_effects: call.actionStarted ? "possible" : "none" } };
  }

  private async respond(call: Call, response: Response): Promise<void> {
    if (call.terminal) return;
    if (Buffer.byteLength(JSON.stringify(response)) + 1 > call.request.max_result_bytes) {
      const overflow = this.error(call, new OperationFailure("RESULT_TOO_LARGE", "serialize", "The completed operation exceeds the response byte budget."), this.ownedSession(call.request));
      if (call.actionStarted) overflow.error.local_execution = "completed";
      response = overflow;
    }
    assertResponseMatches(call.request, response);
    call.terminal = true;
    await this.output(response);
  }

  private ownedSession(request: Request): Session | undefined {
    const session = this.sessions.get(request.session_id);
    return session && sameOwner(session.owner, request.owner) ? session : undefined;
  }

  private async execute(call: Call, valid: boolean, remaining: number): Promise<void> {
    const request = call.request;
    let session = this.ownedSession(request);
    let timer: NodeJS.Timeout | undefined;
    try {
      if (!valid) throw new OperationFailure("INVALID_ARGUMENT", "validate", "Tool parameters do not match the protocol schema.");
      const control = request.tool === "cancel" || request.tool === "close";
      if (remaining <= 0) throw new OperationFailure("DEADLINE_EXCEEDED", "validate", "The call deadline has expired.");
      if (remaining > (control ? 5000 : 120_000)) throw new OperationFailure("INVALID_ARGUMENT", "validate", "The deadline exceeds this operation's allowed budget.");
      if (control) { await this.control(call, session); return; }
      if (request.tool === "open" && request.params.mode === "create") {
        if (this.sessions.has(request.session_id)) throw new OperationFailure(session ? "SESSION_CONFLICT" : "SESSION_UNAVAILABLE", "session", "This session cannot be created.");
        session = this.createSession(call, request);
      }
      if (!session) throw new OperationFailure("SESSION_UNAVAILABLE", "session", "The session is unavailable for this owner.");
      if (session.state === "closing" || session.state === "closed") throw new OperationFailure("SESSION_CLOSING", "session", "The session is closing or closed.");
      if (session.active) throw new OperationFailure("CAPACITY_EXCEEDED", "session", "One ordinary call is already active in this session.");
      if (call.expiresAt > session.expiresAt) throw new OperationFailure("INVALID_ARGUMENT", "validate", "The call deadline exceeds the session lifetime.");
      session.active = call;
      const current = session;
      timer = setTimeout(() => {
        call.abort.abort(new OperationFailure("DEADLINE_EXCEEDED", this.stage(request), "The operation exceeded its deadline; cleanup was requested."));
        void this.closeSession(current).catch(() => undefined);
      }, Math.max(1, call.expiresAt - performance.now()));
      const cancelled = new Promise<never>((_, reject) => {
        call.abort.signal.addEventListener("abort", () => reject(call.abort.signal.reason), { once: true });
      });
      const result = await Promise.race([this.perform(call, session), cancelled]);
      call.abort.signal.throwIfAborted();
      if (performance.now() >= call.expiresAt) throw new OperationFailure("DEADLINE_EXCEEDED", this.stage(request), "The result arrived after the deadline.");
      await this.respond(call, { ...this.base(call), type: "result", data: result.data, artifacts: result.artifacts });
    } catch (error) {
      const failure = call.abort.signal.aborted ? call.abort.signal.reason as OperationFailure : error instanceof OperationFailure ? error
        : new OperationFailure(this.browser && !this.browser.isConnected() ? "BROWSER_DISCONNECTED" : this.failureCode(request), this.stage(request), "The browser operation failed; no application verdict is implied.");
      if (session && session.active === call && request.tool === "open" && call.actionStarted) void this.closeSession(session).catch(() => undefined);
      await this.respond(call, this.error(call, failure, session));
    } finally {
      if (timer) clearTimeout(timer);
      if (session?.active === call) delete session.active;
    }
  }

  private createSession(call: Call, request: Extract<Request, { tool: "open" }>): Session {
    if (request.params.mode !== "create") throw new Error("unexpected_mode");
    const { policy, expires_at } = request.params;
    const allowed = this.config.fixtureOwners.some((owner) => owner.tenant_id === request.owner.tenant_id && owner.run_id === request.owner.run_id);
    if (!allowed || policy.navigation_origins.length !== 1 || policy.navigation_origins[0] !== this.config.fixtureOrigin
      || policy.allow_public_subresources || policy.private_target_ids.length !== 1 || policy.private_target_ids[0] !== "r1_fixture") {
      throw new OperationFailure("POLICY_DENIED", "validate", "This R1 process only accepts its explicitly bound local fixture.");
    }
    this.requireFixtureUrl(request.params.url);
    const lifetime = Date.parse(expires_at) - Date.now();
    if (lifetime <= 0 || lifetime > 1_800_000 || Date.parse(request.deadline_at) > Date.parse(expires_at)) {
      throw new OperationFailure("INVALID_ARGUMENT", "validate", "The session lifetime is invalid.");
    }
    if (this.sessions.size >= 16 || [...this.sessions.values()].filter((s) => s.state !== "closed").length >= 2) {
      throw new OperationFailure("CAPACITY_EXCEEDED", "session", "The R1 session limit was reached.");
    }
    // Reuse the call's clock anchor so equal UTC deadlines stay equal after validation.
    const expiresAt = call.expiresAt + (Date.parse(expires_at) - Date.parse(request.deadline_at));
    const session: Session = { id: request.session_id, owner: structuredClone(request.owner), state: "creating",
      expiresAt, console: new ObservationLog(), network: new ObservationLog() };
    this.sessions.set(session.id, session);
    session.expiryTimer = setTimeout(() => {
      session.active?.abort.abort(new OperationFailure("DEADLINE_EXCEEDED", "session", "The session lifetime expired."));
      void this.closeSession(session).catch(() => undefined);
    }, Math.max(1, session.expiresAt - performance.now()));
    return session;
  }

  private requireFixtureUrl(raw: string): void {
    const url = new URL(raw);
    if (url.origin !== this.config.fixtureOrigin || url.username || url.password || url.search || url.hash
      || !["/", "/game", "/slow", "/missing", "/state"].includes(url.pathname)) {
      throw new OperationFailure("POLICY_DENIED", "validate", "The URL is outside this experiment's fixed fixture routes.");
    }
  }

  private async sharedBrowser(): Promise<Browser> {
    this.browserPromise ??= chromium.launch({ executablePath: this.config.chromiumPath, headless: true, timeout: 15_000,
      env: { PATH: process.env.PATH ?? "/usr/bin:/bin", TMPDIR: process.env.TMPDIR ?? this.config.spoolRoot },
      args: ["--disable-background-networking", "--disable-component-update", "--disable-crash-reporter"] }).then((browser) => {
      this.browser = browser;
      return browser;
    });
    return this.browserPromise;
  }

  private async allocate(session: Session, request: Extract<Request, { tool: "open" }>): Promise<void> {
    if (request.params.mode !== "create") throw new Error("unexpected_mode");
    const browser = await this.sharedBrowser();
    if (session.state === "closing" || session.state === "closed") return;
    const context = await browser.newContext({ viewport: { width: request.params.viewport.width_px, height: request.params.viewport.height_px },
      deviceScaleFactor: 1, acceptDownloads: false, serviceWorkers: "block", permissions: [] });
    session.context = context;
    if (this.contextState(session) === "closing" || this.contextState(session) === "closed") { await context.close(); return; }
    await context.route("**/*", async (route) => {
      try { this.requireFixtureUrl(route.request().url()); }
      catch { await route.abort("blockedbyclient"); return; }
      await route.continue();
    });
    await context.routeWebSocket("**/*", (socket) => socket.close());
    const page = await context.newPage();
    session.page = page;
    context.on("page", (other) => { if (other !== page) void other.close().catch(() => undefined); });
    page.on("dialog", (dialog) => { void dialog.dismiss().catch(() => undefined); });
    page.on("download", (download) => { void download.cancel().catch(() => undefined); });
    page.on("filechooser", (chooser) => { void chooser.setFiles([]).catch(() => undefined); });
    page.on("console", (message) => {
      const level = message.type();
      const raw = message.text();
      session.console.append({ level: ["debug", "info", "log", "warning", "error"].includes(level) ? level : "log",
        text: redact(raw, this.config.knownSecrets), truncated: [...raw].length > 2048 });
    });
    page.on("pageerror", () => session.console.append({ level: "error", text: "An uncaught page error was observed.", truncated: false }));
    page.on("response", (response) => {
      const request = response.request();
      const kind = request.resourceType();
      session.network.append({ url: redactedUrl(request.url()), method: request.method().slice(0, 16),
        resource_type: ["document", "stylesheet", "image", "media", "font", "script", "xhr", "fetch"].includes(kind) ? kind : "other",
        status_code: response.status(), failure: null, duration_ms: null });
    });
    page.on("requestfailed", (request) => {
      if (!request.url().startsWith("http")) return;
      session.network.append({ url: redactedUrl(request.url()), method: request.method().slice(0, 16), resource_type: "other",
        status_code: null, failure: "other", duration_ms: null });
    });
  }

  private async perform(call: Call, session: Session): Promise<{ data: Record<string, unknown>; artifacts: Artifact[] }> {
    const request = call.request;
    if (request.tool === "open" && request.params.mode === "create") {
      call.actionStarted = true;
      session.allocation = this.allocate(session, request);
      await session.allocation;
    }
    call.abort.signal.throwIfAborted();
    const page = session.page;
    if (!page || (session.state !== "creating" && session.state !== "open")) throw new OperationFailure("SESSION_CLOSING", "session", "This session cannot perform an ordinary call.");
    const timeout = Math.max(1, call.expiresAt - performance.now());
    switch (request.tool) {
      case "open": {
        this.requireFixtureUrl(request.params.url);
        call.actionStarted = true;
        await page.goto(request.params.url, { waitUntil: "domcontentloaded", timeout });
        call.abort.signal.throwIfAborted();
        if (this.contextState(session) === "closing" || this.contextState(session) === "closed") throw new OperationFailure("SESSION_CLOSING", "session", "Session closure superseded navigation.");
        session.state = "open";
        return { data: { context_state: "open", page: await describePage(page, this.config.knownSecrets) }, artifacts: [] };
      }
      case "observe": {
        call.actionStarted = true;
        const dom = request.params.include_dom ? await captureDom(page, this.config.knownSecrets, Math.min(16_384, Math.floor(request.max_result_bytes / 8))) : null;
        let artifact: Artifact | undefined;
        if (request.params.include_screenshot) {
          const bytes = await page.screenshot({ type: "png", fullPage: false, timeout,
            mask: [page.locator("input,textarea,[data-private]")], maskColor: "#000000" });
          call.abort.signal.throwIfAborted();
          try { artifact = await stageScreenshot(this.config.spoolRoot, session.id, bytes); }
          catch { throw new OperationFailure("ARTIFACT_FAILED", "observe", "The screenshot staging file could not be completed."); }
        }
        return { data: { page: await describePage(page, this.config.knownSecrets), dom, screenshot_handle: artifact?.handle ?? null }, artifacts: artifact ? [artifact] : [] };
      }
      case "mouse": {
        const p = request.params;
        if ("x_px" in p && (p.x_px >= page.viewportSize()!.width || p.y_px >= page.viewportSize()!.height)) {
          throw new OperationFailure("INVALID_ARGUMENT", "input", "The input coordinate is outside the viewport.");
        }
        call.actionStarted = true;
        if (p.action === "click") await page.mouse.click(p.x_px, p.y_px, { button: p.button, clickCount: p.click_count });
        else if (p.action === "move") await page.mouse.move(p.x_px, p.y_px);
        else if (p.action === "wheel") await page.mouse.wheel(p.delta_x_px, p.delta_y_px);
        else if (p.action === "down") await page.mouse.down({ button: p.button });
        else await page.mouse.up({ button: p.button });
        return { data: { dispatched: true }, artifacts: [] };
      }
      case "keyboard": {
        call.actionStarted = true;
        const p = request.params;
        if (p.action === "type") await page.keyboard.insertText(p.text);
        else await page.keyboard[p.action](p.key === "Space" ? " " : p.key);
        return { data: { dispatched: true }, artifacts: [] };
      }
      case "console": case "network": {
        const log = session[request.tool];
        let limit = request.params.limit;
        let data = log.read(request.params.after_sequence, limit);
        while (limit > 0 && Buffer.byteLength(JSON.stringify({ ...this.base(call), type: "result", data })) + 1 > request.max_result_bytes) {
          data = log.read(request.params.after_sequence, --limit);
        }
        return { data, artifacts: [] };
      }
      default: throw new Error("control_reached_ordinary_dispatch");
    }
  }

  private async control(call: Call, session: Session | undefined): Promise<void> {
    const request = call.request;
    if (!session) throw new OperationFailure("SESSION_UNAVAILABLE", "session", "The session is unavailable for this owner.");
    if (request.tool === "cancel") {
      const target = this.calls.get(request.params.target_request_id);
      if (!target || target.request.call_id !== request.params.target_call_id || target.request.session_id !== session.id
        || !sameOwner(target.request.owner, request.owner) || ["cancel", "close"].includes(target.request.tool)) {
        throw new OperationFailure("CANCEL_TARGET_INVALID", "validate", "The cancellation target does not match an ordinary call in this session.");
      }
      const disposition = target.terminal ? "already_terminal" : "accepted";
      session.active?.abort.abort(new OperationFailure("CANCELLED", this.stage(target.request), "Cancellation accepted; local termination may still be pending."));
      void this.closeSession(session).catch(() => undefined);
      await this.respond(call, { ...this.base(call), type: "result", data: { target_request_id: request.params.target_request_id,
        target_call_id: request.params.target_call_id, disposition, context_state: session.state } });
      return;
    }
    session.active?.abort.abort(new OperationFailure("CANCELLED", "cleanup", "Context closure was requested."));
    let timer: NodeJS.Timeout | undefined;
    try {
      await Promise.race([this.closeSession(session), new Promise<never>((_, reject) => {
        timer = setTimeout(() => reject(new OperationFailure("DEADLINE_EXCEEDED", "cleanup", "Context closure could not be confirmed within the cleanup budget.")), Math.max(1, call.expiresAt - performance.now()));
      })]);
      await this.respond(call, { ...this.base(call), type: "result", data: { context_state: "closed" } });
    } finally { if (timer) clearTimeout(timer); }
  }

  private closeSession(session: Session): Promise<void> {
    if (session.closing) return session.closing;
    session.state = "closing";
    if (session.expiryTimer) clearTimeout(session.expiryTimer);
    session.closing = (async () => {
      // Wait for allocation, not navigation. Late-created Contexts cannot escape cleanup.
      await session.allocation?.catch(() => undefined);
      if (session.context) await session.context.close();
      session.state = "closed";
    })();
    return session.closing;
  }

  private stage(request: Request): Failure["stage"] {
    return request.tool === "open" ? "navigate" : request.tool === "observe" ? "observe"
      : request.tool === "mouse" || request.tool === "keyboard" ? "input"
        : request.tool === "close" || request.tool === "cancel" ? "cleanup" : "read";
  }
  private failureCode(request: Request): string {
    return request.tool === "open" ? "NAVIGATION_FAILED" : request.tool === "observe" ? "OBSERVATION_FAILED"
      : request.tool === "mouse" || request.tool === "keyboard" ? "INPUT_FAILED" : "INTERNAL_ERROR";
  }

  shutdown(): Promise<void> {
    if (this.shutdownPromise) return this.shutdownPromise;
    this.stopping = true;
    this.shutdownPromise = (async () => {
      for (const session of this.sessions.values()) {
        session.active?.abort.abort(new OperationFailure("CANCELLED", "cleanup", "The JSONL process is shutting down."));
      }
      const closed = await Promise.allSettled([...this.sessions.values()].map((session) => this.closeSession(session)));
      try { if (this.browserPromise) await (await this.browserPromise).close(); }
      finally { await Promise.allSettled([...this.tasks]); }
      if (closed.some((result) => result.status === "rejected")) throw new Error("context_cleanup_unconfirmed");
    })();
    return this.shutdownPromise;
  }
}
