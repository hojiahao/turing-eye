//! 在实现接口前验证已确认契约的传输类型。

use serde::{Deserialize, Serialize};
use turing_eye_review::plans::{DimensionConfig, PlatformDimensions, ValidationIssue};
pub use turing_eye_review::scoring::{Score, WeightRatio};

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DimensionInput {
    pub dimension_key: String,
    pub title: String,
    pub weight_ratio: WeightRatio,
    pub ai_weight_ratio: WeightRatio,
    pub human_weight_ratio: WeightRatio,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChallengeInput {
    pub title: String,
    pub requirements: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub competition_title: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CreatePlanRequest {
    pub challenge: ChallengeInput,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub dimensions: Vec<DimensionInput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_thresholds: Option<serde_json::Value>,
}

impl CreatePlanRequest {
    /// 结构解析之后执行平台维度业务校验；不生成指标或创建方案任务。
    pub fn validate_dimensions(&self) -> Result<PlatformDimensions, Vec<ValidationIssue>> {
        PlatformDimensions::new(
            self.dimensions
                .iter()
                .map(|dimension| DimensionConfig {
                    dimension_key: dimension.dimension_key.clone(),
                    title: dimension.title.clone(),
                    weight_ratio: dimension.weight_ratio,
                    ai_weight_ratio: dimension.ai_weight_ratio,
                    human_weight_ratio: dimension.human_weight_ratio,
                })
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::WeightRatio;

    #[test]
    fn every_basis_point_has_an_exact_round_trip() {
        for points in 0..=10_000 {
            let ratio = WeightRatio::from_basis_points(points).unwrap();
            let encoded = serde_json::to_string(&ratio).unwrap();
            let decoded: WeightRatio = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded.basis_points(), points);
        }
    }

    #[test]
    fn ratios_reject_wrong_units_precision_and_newlines() {
        for value in [
            "0.2",
            "20.00",
            "1.0001",
            "0.20000",
            "0.2000\n",
            "0.٢٠٠٠",
            "NaN",
            "-0.1000",
        ] {
            assert!(serde_json::from_value::<WeightRatio>(value.into()).is_err());
        }
        assert!(serde_json::from_str::<WeightRatio>("0.2").is_err());
        assert!(serde_json::from_str::<WeightRatio>("null").is_err());
    }
}
