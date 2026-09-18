import type { components, paths } from '../src/api/generated.js';

type Schema = components['schemas'];

// Consumers preserve wire names, nulls, and the platform's dimension order.
export function summarizeInputs(input: Schema['CreatePlanRequest']) {
  return input.dimensions.map((dimension) => ({
    key: dimension.dimension_key,
    title: dimension.title,
    ratio: dimension.weight_ratio,
  }));
}

export function renderIndicator(item: Schema['IndicatorScore']) {
  return { score: item.score, evidence: item.evidence_refs };
}

export type CreateResponse = paths['/v2/reviews']['post']['responses'][202]['content']['application/json'];
export type RuntimeEvent = Schema['RuntimeEvent'];
export type Report = Schema['ReportDocument'];
export type HumanInput = Schema['HumanResultsRequest'];
