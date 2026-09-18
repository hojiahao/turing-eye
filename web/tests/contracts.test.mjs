import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import Ajv2020 from 'ajv/dist/2020.js';
import addFormats from 'ajv-formats';

const read = (path) => JSON.parse(readFileSync(new URL(path, import.meta.url), 'utf8'));
const document = read('../../contracts/openapi.json');
const samples = read('../../contracts/examples.json');
const tools = read('../../contracts/tool-protocol.schema.json');
const ajv = new Ajv2020({ strict: false, allErrors: true, multipleOfPrecision: 8 });
addFormats(ajv);

for (const sample of samples.cases) {
  test(`${sample.expect}: ${sample.name}`, () => {
    const validate = ajv.compile({ $ref: `#/components/schemas/${sample.schema}`, components: document.components });
    assert.equal(validate(sample.value), sample.expect !== 'schema_invalid', JSON.stringify(validate.errors));
  });
}

test('every HTTP component resolves in the TypeScript runtime', () => {
  for (const name of Object.keys(document.components.schemas)) {
    ajv.compile({ $ref: `#/components/schemas/${name}`, components: document.components });
  }
});

test('browser protocol examples cover all eight tools', () => {
  const validate = ajv.compile(tools);
  const observed = new Set();
  for (const frame of tools.examples) {
    assert.ok(validate(frame), JSON.stringify(validate.errors));
    observed.add(frame.tool);
    assert.equal(validate({ ...frame, hidden_tool: 'shell' }), false);
  }
  assert.equal(observed.size, 8);
});
