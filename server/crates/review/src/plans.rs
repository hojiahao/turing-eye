//! 平台维度配置的纯校验。保留输入数量、顺序、键、名称和比例，不补默认值。

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::scoring::WeightRatio;

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DimensionConfig {
    pub dimension_key: String,
    pub title: String,
    pub weight_ratio: WeightRatio,
    pub ai_weight_ratio: WeightRatio,
    pub human_weight_ratio: WeightRatio,
}

/// 明细码用于 API 的 errors；HTTP 状态及顶层问题类型由入口负责映射。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ValidationCode {
    RequiredField,
    InvalidValue,
    InvalidText,
    DuplicateDimensionKey,
    DuplicateIndicatorKey,
    InvalidDimensionWeights,
    InvalidChannelWeights,
    InvalidIndicatorWeights,
    UnknownDimensionKey,
    UnknownIndicatorKey,
    HumanDimensionNotAllowed,
    InvalidDecisionThresholds,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ValidationIssue {
    pub code: ValidationCode,
    pub path: String,
}

impl ValidationIssue {
    pub(crate) fn new(code: ValidationCode, path: impl Into<String>) -> Self {
        Self {
            code,
            path: path.into(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct PlatformDimensions {
    dimensions: Vec<DimensionConfig>,
}

impl PlatformDimensions {
    pub fn new(dimensions: Vec<DimensionConfig>) -> Result<Self, Vec<ValidationIssue>> {
        let mut issues = Vec::new();
        if dimensions.is_empty() {
            issues.push(ValidationIssue::new(
                ValidationCode::RequiredField,
                "dimensions",
            ));
        }
        let mut keys = HashSet::new();
        let mut total = 0_u128;
        for (index, dimension) in dimensions.iter().enumerate() {
            let path = format!("dimensions.{index}");
            if !valid_scoring_key(&dimension.dimension_key) {
                issues.push(ValidationIssue::new(
                    ValidationCode::InvalidValue,
                    format!("{path}.dimension_key"),
                ));
            }
            if !keys.insert(&dimension.dimension_key) {
                issues.push(ValidationIssue::new(
                    ValidationCode::DuplicateDimensionKey,
                    format!("{path}.dimension_key"),
                ));
            }
            if !valid_title(&dimension.title) {
                issues.push(ValidationIssue::new(
                    ValidationCode::InvalidText,
                    format!("{path}.title"),
                ));
            }
            if u32::from(dimension.ai_weight_ratio.basis_points())
                + u32::from(dimension.human_weight_ratio.basis_points())
                != 10_000
            {
                issues.push(ValidationIssue::new(
                    ValidationCode::InvalidChannelWeights,
                    path,
                ));
            }
            total += u128::from(dimension.weight_ratio.basis_points());
        }
        if total != 10_000 {
            issues.push(ValidationIssue::new(
                ValidationCode::InvalidDimensionWeights,
                "dimensions",
            ));
        }
        if !issues.is_empty() {
            return Err(issues);
        }
        Ok(Self { dimensions })
    }

    pub fn dimensions(&self) -> &[DimensionConfig] {
        &self.dimensions
    }

    pub fn requires_ai(&self) -> bool {
        self.dimensions.iter().any(|dimension| {
            dimension.weight_ratio.basis_points() > 0
                && dimension.ai_weight_ratio.basis_points() > 0
        })
    }

    pub fn required_human_dimensions(&self) -> impl Iterator<Item = &DimensionConfig> {
        self.dimensions.iter().filter(|dimension| {
            dimension.weight_ratio.basis_points() > 0
                && dimension.human_weight_ratio.basis_points() > 0
        })
    }
}

pub(crate) fn valid_scoring_key(key: &str) -> bool {
    let bytes = key.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'_')
}

fn valid_title(title: &str) -> bool {
    !title.trim().is_empty()
        && title.chars().count() <= 120
        && !title.chars().any(|character| {
            (character <= '\u{1f}' && !matches!(character, '\n' | '\r' | '\t'))
                || character == '\u{7f}'
        })
}
