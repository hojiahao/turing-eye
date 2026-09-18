import { test } from "node:test";
import assert from "node:assert/strict";
import { chromium, type Browser } from "playwright-core";
import { packageRoot, type Owner, type Request, type Response } from "../src/protocol.js";
import { BrowserWorker } from "../src/worker.js";

for (const exceedsLifetime of [false, true]) {
  test(exceedsLifetime ? "creation rejects a call deadline beyond the session lifetime"
    : "equal call and session deadlines remain valid after validation takes time", async (t) => {
    const receivedAtUnixMs = Date.now();
    let clockReads = 0;
    t.mock.method(Date, "now", () => receivedAtUnixMs + (clockReads++ === 0 ? 0 : 10));
    // Stop at allocation: this unit regression checks validation without launching Chromium.
    const browser = {
      newContext: async () => { throw new Error("injected_allocation_failure"); },
      isConnected: () => true,
      close: async () => {},
    } as unknown as Browser;
    const launch = t.mock.method(chromium, "launch", async () => browser);
    const owner: Owner = { tenant_id: "tenant_r1", run_id: "run_deadline", task_id: "task_play",
      attempt_id: "attempt_deadline", attempt_generation: 1 };
    const fixtureOrigin = "http://127.0.0.1:1234";
    const reply = Promise.withResolvers<Response>();
    const worker = new BrowserWorker({ chromiumPath: "/not-launched", spoolRoot: packageRoot,
      fixtureOrigin, fixtureOwners: [owner], knownSecrets: [] }, async (frame) => reply.resolve(frame));
    const request: Request = { protocol_version: "2.0", type: "request", request_id: "req_deadline", call_id: "call_deadline",
      owner, session_id: "session_deadline", max_result_bytes: 65_536, tool: "open",
      deadline_at: new Date(receivedAtUnixMs + 5000 + (exceedsLifetime ? 1 : 0)).toISOString(),
      params: { mode: "create", url: `${fixtureOrigin}/game`, expires_at: new Date(receivedAtUnixMs + 5000).toISOString(),
        policy: { revision: "policy_r1", navigation_origins: [fixtureOrigin], allow_public_subresources: false, private_target_ids: ["r1_fixture"] },
        viewport: { width_px: 640, height_px: 480 } } };
    try {
      worker.accept(request);
      const response = await reply.promise;
      assert.equal(response.type, "error");
      if (response.type !== "error") assert.fail("Expected validation or injected allocation failure");
      assert.equal(response.error.code, exceedsLifetime ? "INVALID_ARGUMENT" : "NAVIGATION_FAILED");
      assert.equal(launch.mock.callCount(), exceedsLifetime ? 0 : 1);
    } finally {
      await worker.shutdown();
    }
  });
}
