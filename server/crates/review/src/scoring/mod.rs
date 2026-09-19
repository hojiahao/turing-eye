//! 从同一冻结规则与有效评分计算结果；不生成评语，也不执行外部 I/O。

mod values;

use std::collections::HashMap;

use serde::Serialize;

use crate::plans::{
    DimensionConfig, PlatformDimensions, ValidationCode, ValidationIssue, valid_scoring_key,
};
use values::BASIS;
pub use values::{InvalidNumber, Score, WeightRatio};

#[derive(Clone, Debug)]
pub struct IndicatorWeight {
    pub indicator_key: String,
    pub weight_ratio: WeightRatio,
}

#[derive(Clone, Debug)]
pub struct DimensionIndicators {
    pub dimension_key: String,
    pub indicators: Vec<IndicatorWeight>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MissingCategory {
    Pending,
    Input,
    Contestant,
    Service,
    Provider,
    Budget,
    Cancelled,
}

/// 只接收判定层已确认的分数。判定依据、证据登记和持久化由所属模块负责。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IndicatorResult {
    Scored(Score),
    Pending,
    Unavailable {
        category: MissingCategory,
        reason_code: String,
    },
}

#[derive(Clone, Debug)]
pub struct AiIndicatorResult {
    pub dimension_key: String,
    pub indicator_key: String,
    pub result: IndicatorResult,
}

#[derive(Clone, Debug)]
pub struct HumanDimensionResult {
    pub dimension_key: String,
    pub score: Score,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScoreStatus {
    Pending,
    Partial,
    Complete,
    Unavailable,
    NotRequired,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SummaryStatus {
    Pending,
    Partial,
    Complete,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScoreChannel {
    Ai,
    Human,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MissingScore {
    pub dimension_key: String,
    pub indicator_key: Option<String>,
    pub channel: ScoreChannel,
    pub category: MissingCategory,
    pub reason_code: String,
}

#[derive(Clone, Debug)]
pub struct CalculatedIndicator {
    pub indicator_key: String,
    pub weight_ratio: WeightRatio,
    /// 未要求且没有结果时为 None；必要项尚无结果时显式为 Pending。
    pub result: Option<IndicatorResult>,
}

#[derive(Clone, Debug)]
pub struct CalculatedDimension {
    pub configuration: DimensionConfig,
    pub indicators: Vec<CalculatedIndicator>,
    pub score_status: ScoreStatus,
    pub ai_score: Option<Score>,
    pub human_score: Option<Score>,
    pub final_score: Option<Score>,
}

#[derive(Clone, Debug)]
pub struct ScoreCalculation {
    pub dimensions: Vec<CalculatedDimension>,
    pub ai_score: Option<Score>,
    pub score_status: ScoreStatus,
    pub final_score: Option<Score>,
    pub summary_status: SummaryStatus,
    pub missing_scores: Vec<MissingScore>,
}

#[derive(Clone, Debug)]
pub struct ScoringRules {
    platform: PlatformDimensions,
    indicators: Vec<Vec<IndicatorWeight>>,
}

impl ScoringRules {
    /// 校验计分结构，不代替方案草稿的文本、归属及定稿修订检查。
    pub fn new(
        platform: PlatformDimensions,
        definitions: Vec<DimensionIndicators>,
    ) -> Result<Self, Vec<ValidationIssue>> {
        let mut issues = Vec::new();
        let mut definitions_by_key = HashMap::new();
        for (index, definition) in definitions.iter().enumerate() {
            let path = format!("dimensions.{index}");
            if definitions_by_key
                .insert(
                    definition.dimension_key.as_str(),
                    (index, &definition.indicators),
                )
                .is_some()
            {
                issues.push(ValidationIssue::new(
                    ValidationCode::DuplicateDimensionKey,
                    format!("{path}.dimension_key"),
                ));
            }
            if !platform
                .dimensions()
                .iter()
                .any(|dimension| dimension.dimension_key == definition.dimension_key)
            {
                issues.push(ValidationIssue::new(
                    ValidationCode::UnknownDimensionKey,
                    format!("{path}.dimension_key"),
                ));
            }
        }
        let mut indicators = Vec::new();
        for (index, dimension) in platform.dimensions().iter().enumerate() {
            let Some((definition_index, items)) =
                definitions_by_key.get(dimension.dimension_key.as_str())
            else {
                issues.push(ValidationIssue::new(
                    ValidationCode::RequiredField,
                    format!("platform.dimensions.{index}.indicators"),
                ));
                continue;
            };
            let path = format!("dimensions.{definition_index}.indicators");
            let mut keys = std::collections::HashSet::new();
            let mut total = 0_u128;
            for (item_index, item) in items.iter().enumerate() {
                let key_path = format!("{path}.{item_index}.indicator_key");
                if !valid_scoring_key(&item.indicator_key) {
                    issues.push(ValidationIssue::new(
                        ValidationCode::InvalidValue,
                        &key_path,
                    ));
                }
                if !keys.insert(&item.indicator_key) {
                    issues.push(ValidationIssue::new(
                        ValidationCode::DuplicateIndicatorKey,
                        key_path,
                    ));
                }
                total += u128::from(item.weight_ratio.basis_points());
            }
            if total != BASIS {
                issues.push(ValidationIssue::new(
                    ValidationCode::InvalidIndicatorWeights,
                    path,
                ));
            }
            indicators.push((*items).clone());
        }
        if !issues.is_empty() {
            return Err(issues);
        }
        Ok(Self {
            platform,
            indicators,
        })
    }

    pub fn configuration(&self) -> &PlatformDimensions {
        &self.platform
    }

    /// 允许部分已存结果；完整人工提交的原子校验由人工接收用例负责。
    pub fn calculate(
        &self,
        ai: &[AiIndicatorResult],
        human: &[HumanDimensionResult],
    ) -> Result<ScoreCalculation, Vec<ValidationIssue>> {
        let mut issues = Vec::new();
        let dimensions_by_key: HashMap<_, _> = self
            .platform
            .dimensions()
            .iter()
            .enumerate()
            .map(|(index, dimension)| (dimension.dimension_key.as_str(), index))
            .collect();
        let mut ai_by_key = HashMap::new();
        for (index, entry) in ai.iter().enumerate() {
            let path = format!("ai.{index}");
            let Some(&dimension_index) = dimensions_by_key.get(entry.dimension_key.as_str()) else {
                issues.push(ValidationIssue::new(
                    ValidationCode::UnknownDimensionKey,
                    format!("{path}.dimension_key"),
                ));
                continue;
            };
            if !self.indicators[dimension_index]
                .iter()
                .any(|item| item.indicator_key == entry.indicator_key)
            {
                issues.push(ValidationIssue::new(
                    ValidationCode::UnknownIndicatorKey,
                    format!("{path}.indicator_key"),
                ));
            }
            if ai_by_key
                .insert(
                    (entry.dimension_key.as_str(), entry.indicator_key.as_str()),
                    &entry.result,
                )
                .is_some()
            {
                issues.push(ValidationIssue::new(
                    ValidationCode::DuplicateIndicatorKey,
                    format!("{path}.indicator_key"),
                ));
            }
            if let IndicatorResult::Unavailable {
                category,
                reason_code,
            } = &entry.result
                && (*category == MissingCategory::Pending || reason_code.trim().is_empty())
            {
                issues.push(ValidationIssue::new(
                    ValidationCode::InvalidValue,
                    format!("{path}.result"),
                ));
            }
        }
        let mut human_by_key = HashMap::new();
        for (index, entry) in human.iter().enumerate() {
            let path = format!("human.{index}.dimension_key");
            let Some(&dimension_index) = dimensions_by_key.get(entry.dimension_key.as_str()) else {
                issues.push(ValidationIssue::new(
                    ValidationCode::UnknownDimensionKey,
                    path,
                ));
                continue;
            };
            if self.platform.dimensions()[dimension_index]
                .human_weight_ratio
                .basis_points()
                == 0
            {
                issues.push(ValidationIssue::new(
                    ValidationCode::HumanDimensionNotAllowed,
                    &path,
                ));
            }
            if human_by_key
                .insert(entry.dimension_key.as_str(), entry.score)
                .is_some()
            {
                issues.push(ValidationIssue::new(
                    ValidationCode::DuplicateDimensionKey,
                    path,
                ));
            }
        }
        if !issues.is_empty() {
            return Err(issues);
        }

        let mut dimensions = Vec::new();
        let mut missing_scores = Vec::new();
        let mut ai_total_numerator = 0_u128;
        let mut ai_total_coefficient = 0_u128;
        let mut final_total_numerator = 0_u128;
        let mut ai_count = Completion::default();
        let mut human_scored = 0;
        let mut summary_complete = true;

        for (dimension, items) in self.platform.dimensions().iter().zip(&self.indicators) {
            let key = dimension.dimension_key.as_str();
            let weight = u128::from(dimension.weight_ratio.basis_points());
            let ai_weight = u128::from(dimension.ai_weight_ratio.basis_points());
            let human_weight = u128::from(dimension.human_weight_ratio.basis_points());
            let required_ai = weight * ai_weight > 0;
            let mut count = Completion::default();
            let mut ai_numerator = 0_u128;
            let mut indicators = Vec::new();
            for item in items {
                let ratio = u128::from(item.weight_ratio.basis_points());
                let required = required_ai && ratio > 0;
                let result = ai_by_key.get(&(key, item.indicator_key.as_str())).copied();
                if ratio > 0 {
                    match result {
                        Some(IndicatorResult::Scored(score)) => {
                            count.scored += 1;
                            ai_numerator += u128::from(score.hundredths()) * ratio;
                        }
                        Some(IndicatorResult::Unavailable {
                            category,
                            reason_code,
                        }) => {
                            count.unavailable += 1;
                            if required {
                                missing_scores.push(missing_ai(
                                    key,
                                    &item.indicator_key,
                                    *category,
                                    reason_code,
                                ));
                            }
                        }
                        _ => {
                            count.pending += 1;
                            if required {
                                missing_scores.push(missing_ai(
                                    key,
                                    &item.indicator_key,
                                    MissingCategory::Pending,
                                    "SCORE_PENDING",
                                ));
                            }
                        }
                    }
                }
                indicators.push(CalculatedIndicator {
                    indicator_key: item.indicator_key.clone(),
                    weight_ratio: item.weight_ratio,
                    result: result
                        .cloned()
                        .or_else(|| required.then_some(IndicatorResult::Pending)),
                });
            }
            let ai_complete = count.pending == 0 && count.unavailable == 0;
            let ai_score =
                (required_ai && ai_complete).then(|| Score::round_half_up(ai_numerator, BASIS));
            let human_score = human_by_key.get(key).copied();
            if required_ai {
                ai_count.scored += count.scored;
                ai_count.pending += count.pending;
                ai_count.unavailable += count.unavailable;
                ai_total_coefficient += weight * ai_weight;
                ai_total_numerator += weight * ai_weight * ai_numerator;
            }
            if weight * human_weight > 0 {
                if human_score.is_some() {
                    human_scored += 1;
                } else {
                    missing_scores.push(MissingScore {
                        dimension_key: key.to_owned(),
                        indicator_key: None,
                        channel: ScoreChannel::Human,
                        category: MissingCategory::Pending,
                        reason_code: "HUMAN_RESULT_PENDING".to_owned(),
                    });
                }
            }
            let complete =
                (ai_weight == 0 || ai_complete) && (human_weight == 0 || human_score.is_some());
            // A[d] 保留分子，F[d] 保留万分位平方分母；总分不使用展示舍入值。
            let final_numerator = ai_weight * ai_numerator
                + human_weight * u128::from(human_score.map_or(0, Score::hundredths)) * BASIS;
            if weight > 0 {
                summary_complete &= complete;
                final_total_numerator += weight * final_numerator;
            }
            dimensions.push(CalculatedDimension {
                configuration: dimension.clone(),
                indicators,
                score_status: if required_ai {
                    count.status()
                } else {
                    ScoreStatus::NotRequired
                },
                ai_score,
                human_score,
                final_score: complete.then(|| Score::round_half_up(final_numerator, BASIS * BASIS)),
            });
        }
        let score_status = if ai_total_coefficient == 0 {
            ScoreStatus::NotRequired
        } else {
            ai_count.status()
        };
        Ok(ScoreCalculation {
            dimensions,
            score_status,
            ai_score: (score_status == ScoreStatus::Complete)
                .then(|| Score::round_half_up(ai_total_numerator, BASIS * ai_total_coefficient)),
            final_score: summary_complete
                .then(|| Score::round_half_up(final_total_numerator, BASIS * BASIS * BASIS)),
            summary_status: if summary_complete {
                SummaryStatus::Complete
            } else if ai_count.scored > 0 || human_scored > 0 {
                SummaryStatus::Partial
            } else {
                SummaryStatus::Pending
            },
            missing_scores,
        })
    }
}

#[derive(Default)]
struct Completion {
    scored: usize,
    pending: usize,
    unavailable: usize,
}

impl Completion {
    fn status(&self) -> ScoreStatus {
        if self.pending == 0 && self.unavailable == 0 {
            ScoreStatus::Complete
        } else if self.scored > 0 {
            ScoreStatus::Partial
        } else if self.pending > 0 {
            ScoreStatus::Pending
        } else {
            ScoreStatus::Unavailable
        }
    }
}

fn missing_ai(
    dimension: &str,
    indicator: &str,
    category: MissingCategory,
    reason: &str,
) -> MissingScore {
    MissingScore {
        dimension_key: dimension.to_owned(),
        indicator_key: Some(indicator.to_owned()),
        channel: ScoreChannel::Ai,
        category,
        reason_code: reason.to_owned(),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Pass,
    ConditionalPass,
    Fail,
}

#[derive(Clone, Copy, Debug)]
pub struct DecisionThresholds {
    pass_score: Score,
    conditional_pass_score: Score,
}

impl DecisionThresholds {
    pub fn new(pass_score: Score, conditional_pass_score: Score) -> Result<Self, ValidationIssue> {
        if pass_score <= conditional_pass_score {
            return Err(ValidationIssue::new(
                ValidationCode::InvalidDecisionThresholds,
                "decision_thresholds",
            ));
        }
        Ok(Self {
            pass_score,
            conditional_pass_score,
        })
    }

    /// 只对完整的公开舍入分判定；没有阈值时调用方不生成结论。
    pub fn classify(&self, score: Option<Score>) -> Option<Decision> {
        score.map(|score| {
            if score >= self.pass_score {
                Decision::Pass
            } else if score >= self.conditional_pass_score {
                Decision::ConditionalPass
            } else {
                Decision::Fail
            }
        })
    }
}

#[cfg(test)]
mod tests;
