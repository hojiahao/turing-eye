import { readFile } from 'node:fs/promises';
import openapiTS, { astToString } from 'openapi-typescript';

const schema = new URL('../../contracts/openapi.json', import.meta.url);
const generated = new URL('../src/api/generated.ts', import.meta.url);
const expected = astToString(await openapiTS(schema));
const actual = await readFile(generated, 'utf8');
// The CLI adds a header; compare declarations rather than the header.
if (actual.slice(actual.indexOf('export interface paths')) !== expected.slice(expected.indexOf('export interface paths'))) {
  throw new Error('OpenAPI consumer types are stale. Run npm run generate.');
}
