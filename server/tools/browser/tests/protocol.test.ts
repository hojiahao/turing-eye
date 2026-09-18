import { test } from "node:test";
import assert from "node:assert/strict";
import { FrameReader, parseFrame, schema, validateFrame, assertResponseMatches, type Request, type Response } from "../src/protocol.js";

test("the authority schema accepts all 18 golden frames and covers all eight tools", () => {
  const tools = new Set<string>();
  for (const frame of schema.examples) {
    assert.equal(validateFrame(frame), true, JSON.stringify(validateFrame.errors));
    tools.add(frame.tool);
    const changed = structuredClone(frame);
    changed.unexpected = true;
    assert.equal(validateFrame(changed), false);
    const wrongOwner = structuredClone(frame);
    delete wrongOwner.owner.attempt_generation;
    assert.equal(validateFrame(wrongOwner), false);
  }
  assert.equal(schema.examples.length, 18);
  assert.deepEqual([...tools].sort(), ["cancel", "close", "console", "keyboard", "mouse", "network", "observe", "open"]);
});

test("reject every request's tool/parameter mismatch and unknown protocol", () => {
  let count = 0;
  for (const frame of schema.examples.filter((item: { type: string }) => item.type === "request")) {
    const wrong = structuredClone(frame);
    wrong.params = { shell: "disallowed" };
    assert.equal(validateFrame(wrong), false);
    wrong.params = frame.params;
    wrong.protocol_version = "1.0";
    assert.equal(validateFrame(wrong), false);
    count++;
  }
  assert.equal(count, 9);
});

test("strict byte framing rejects duplicate members, malformed UTF-8 and non-JSON values", () => {
  const invalid = ["", "[]", '{"a":1,"a":2}', '{"a":{"b":1,"b":2}}', '{"a":NaN}', '{"a":1e999}',
    '{"a":1,}', '{/*comment*/"a":1}', '{}\r', '\ufeff{}', '{} {}', '{"x":"bad\nline"}'];
  for (const input of invalid) assert.throws(() => parseFrame(Buffer.from(input)), input);
  assert.throws(() => parseFrame(Buffer.from([0xff, 0xfe])));
  assert.throws(() => parseFrame(Buffer.alloc(65_536, 32)));
  assert.throws(() => parseFrame(Buffer.from('{"a":1,"\\u0061":2}')));
  const frame = new FrameReader();
  const actual: unknown[] = [];
  const bytes = Buffer.from('{"value":"中文"}\n{}\n');
  for (const byte of bytes) frame.feed(Buffer.from([byte]), (value) => actual.push(value));
  frame.finish();
  assert.deepEqual(actual, [{ value: "中文" }, {}]);
  const truncated = new FrameReader();
  truncated.feed(Buffer.from("{}"), () => assert.fail("must wait for LF"));
  assert.throws(() => truncated.finish());
  const oversized = new FrameReader();
  oversized.feed(Buffer.alloc(60_000, 32), () => assert.fail());
  assert.throws(() => oversized.feed(Buffer.alloc(6000, 32), () => assert.fail()));
});

test("consumer verifies all correlation fields and artifact/fact links", () => {
  const request = structuredClone(schema.examples[2]) as Request;
  const response = structuredClone(schema.examples[3]) as Response;
  assertResponseMatches(request, response);
  for (const key of ["request_id", "call_id", "session_id", "tool"] as const) {
    const wrong = structuredClone(response);
    Object.assign(wrong, { [key]: key === "tool" ? "close" : "another" });
    assert.throws(() => assertResponseMatches(request, wrong));
  }
  for (const key of ["tenant_id", "run_id", "task_id", "attempt_id", "attempt_generation"] as const) {
    const wrong = structuredClone(response);
    Object.assign(wrong.owner, { [key]: key === "attempt_generation" ? 2 : "another" });
    assert.throws(() => assertResponseMatches(request, wrong));
  }
  const badLink = structuredClone(response);
  badLink.facts[0]!.value_path = "/data/nonexistent";
  assert.throws(() => assertResponseMatches(request, badLink));
  badLink.facts[0]!.value_path = "/data";
  badLink.facts[0]!.artifact_handles = ["not_in_frame"];
  assert.throws(() => assertResponseMatches(request, badLink));
  const badCapture = structuredClone(response);
  badCapture.artifacts = [];
  assert.throws(() => assertResponseMatches(request, badCapture));
});
