import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { resolve } from "node:path";
import { Ajv2020 } from "ajv/dist/2020.js";
import addFormats from "ajv-formats";
import { parseTree, type Node as JsonNode, type ParseError } from "jsonc-parser";

export const packageRoot = fileURLToPath(new URL("../../", import.meta.url));
export const schema = JSON.parse(readFileSync(resolve(packageRoot, "../../../contracts/tool-protocol.schema.json"), "utf8"));
export const maxRequestBytes = 65_536;
const ajv = new Ajv2020({ strict: false, allErrors: false, validateFormats: true });
addFormats.default(ajv);
ajv.addSchema(schema);
export const validateFrame = ajv.getSchema(schema.$id)!;
export const validateRequest = ajv.compile({ $ref: `${schema.$id}#/$defs/Request` });
const envelopeSchema = structuredClone(schema.$defs.Request);
envelopeSchema.properties.params = {};
delete envelopeSchema.allOf;
export const validateEnvelope = ajv.compile({ ...envelopeSchema, $defs: schema.$defs });

export type Owner = {
  tenant_id: string; run_id: string; task_id: string; attempt_id: string; attempt_generation: number;
};
export type Policy = {
  revision: string; navigation_origins: string[]; allow_public_subresources: boolean; private_target_ids: string[];
};
export type ToolParameters = {
  open: { mode: "create"; url: string; policy: Policy; viewport: { width_px: number; height_px: number }; expires_at: string }
    | { mode: "navigate"; url: string };
  observe: { include_dom: boolean; include_screenshot: boolean };
  mouse: { action: "click"; x_px: number; y_px: number; button: "left" | "right" | "middle"; click_count: 1 | 2 }
    | { action: "move"; x_px: number; y_px: number }
    | { action: "down" | "up"; button: "left" | "right" | "middle" }
    | { action: "wheel"; delta_x_px: number; delta_y_px: number };
  keyboard: { action: "press" | "down" | "up"; key: string } | { action: "type"; text: string };
  console: { after_sequence: number; limit: number };
  network: { after_sequence: number; limit: number };
  cancel: { target_request_id: string; target_call_id: string; reason: "run_cancelled" | "deadline_exceeded" | "lease_lost" | "shutdown" };
  close: Record<string, never>;
};
export type Tool = keyof ToolParameters;
export type Request = { [K in Tool]: {
  protocol_version: "2.0"; type: "request"; request_id: string; call_id: string; owner: Owner;
  session_id: string; deadline_at: string; max_result_bytes: number; tool: K; params: ToolParameters[K];
}}[Tool];
export type ContextState = "absent" | "open" | "closing" | "closed" | "unknown";
export type Artifact = { handle: string; media_type: "image/png"; byte_length: number; sha256: string };
export type Failure = {
  code: string; stage: "validate" | "session" | "navigate" | "observe" | "input" | "read" | "cleanup" | "serialize";
  retryable: boolean; detail: string; local_execution: "not_started" | "completed" | "stopped" | "unknown";
  context_state: ContextState; external_effects: "none" | "possible";
};
type ResponseBase = {
  protocol_version: "2.0"; request_id: string; call_id: string; owner: Owner; session_id: string; tool: Tool;
  completed_at: string; duration_ms: number; artifacts: Artifact[];
  facts: { fact_id: string; observed_at: string; kind: string; summary: string; value_path: string; artifact_handles: string[] }[];
};
export type Response = ResponseBase & ({ type: "result"; data: Record<string, unknown> } | { type: "error"; error: Failure });

export class ProtocolFault extends Error {
  constructor(public readonly reason: string) { super(reason); }
}

export function sameOwner(a: Owner, b: Owner): boolean {
  return a.tenant_id === b.tenant_id && a.run_id === b.run_id && a.task_id === b.task_id
    && a.attempt_id === b.attempt_id && a.attempt_generation === b.attempt_generation;
}

export function assertResponseMatches(request: Request, response: Response): void {
  if (!validateFrame(response) || response.request_id !== request.request_id
    || response.call_id !== request.call_id || response.tool !== request.tool || response.session_id !== request.session_id
    || !sameOwner(response.owner, request.owner)) throw new ProtocolFault("response_correlation");
  const handles = new Set(response.artifacts.map((artifact) => artifact.handle));
  if (handles.size !== response.artifacts.length) throw new ProtocolFault("artifact_duplicate");
  for (const fact of response.facts) {
    let value: unknown = response;
    for (const part of fact.value_path.slice(1).split("/")) {
      const key = part.replace(/~1/g, "/").replace(/~0/g, "~");
      if (typeof value !== "object" || value === null || !Object.hasOwn(value, key)) throw new ProtocolFault("fact_pointer");
      value = (value as Record<string, unknown>)[key];
    }
    if (fact.artifact_handles.some((handle) => !handles.has(handle))) throw new ProtocolFault("fact_artifact");
  }
  if (response.type === "result" && request.tool === "observe") {
    if ((response.data.dom !== null) !== request.params.include_dom
      || (response.data.screenshot_handle !== null) !== request.params.include_screenshot
      || (request.params.include_screenshot && !handles.has(String(response.data.screenshot_handle)))) {
      throw new ProtocolFault("observe_correlation");
    }
  }
}

function inspectJson(node: JsonNode, depth = 0): void {
  if (depth > 64) throw new ProtocolFault("json_depth");
  if (node.type === "number" && !Number.isFinite(node.value)) throw new ProtocolFault("json_number");
  if (node.type === "object") {
    const keys = new Set<string>();
    for (const property of node.children ?? []) {
      const key = property.children?.[0]?.value as string;
      if (keys.has(key)) throw new ProtocolFault("json_duplicate_key");
      keys.add(key);
    }
  }
  for (const child of node.children ?? []) inspectJson(child, depth + 1);
}

export function parseFrame(bytes: Buffer): unknown {
  if (bytes.length + 1 > maxRequestBytes) throw new ProtocolFault("frame_too_large");
  if (bytes.length === 0 || bytes.at(-1) === 13 || bytes.subarray(0, 3).equals(Buffer.from([239, 187, 191]))) {
    throw new ProtocolFault("frame_delimiter");
  }
  let text: string;
  try { text = new TextDecoder("utf-8", { fatal: true }).decode(bytes); }
  catch { throw new ProtocolFault("invalid_utf8"); }
  const errors: ParseError[] = [];
  const tree = parseTree(text, errors, { disallowComments: true, allowTrailingComma: false, allowEmptyContent: false });
  if (errors.length || tree?.type !== "object") throw new ProtocolFault("invalid_json");
  inspectJson(tree);
  return JSON.parse(text) as unknown;
}

export class FrameReader {
  private pending = Buffer.alloc(0);
  feed(chunk: Buffer, receive: (frame: unknown) => void): void {
    let start = 0;
    while (start < chunk.length) {
      const newline = chunk.indexOf(10, start);
      const end = newline < 0 ? chunk.length : newline;
      if (this.pending.length + end - start + 1 > maxRequestBytes) throw new ProtocolFault("frame_too_large");
      this.pending = Buffer.concat([this.pending, chunk.subarray(start, end)]);
      if (newline < 0) return;
      const frame = parseFrame(this.pending);
      this.pending = Buffer.alloc(0);
      receive(frame);
      start = newline + 1;
    }
  }
  finish(): void { if (this.pending.length !== 0) throw new ProtocolFault("truncated_frame"); }
}
