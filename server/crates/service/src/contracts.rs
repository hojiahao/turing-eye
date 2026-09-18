//! 在实现接口前验证已确认契约的传输类型。

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

/// 平台传入的四位小数比例，内部按万分位整数保存。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WeightRatio(u16);

impl WeightRatio {
    pub fn basis_points(self) -> u16 {
        self.0
    }
}

impl<'de> Deserialize<'de> for WeightRatio {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        if value == "1.0000" {
            return Ok(Self(10_000));
        }
        let bytes = value.as_bytes();
        if bytes.len() != 6 || &bytes[..2] != b"0." || !bytes[2..].iter().all(u8::is_ascii_digit) {
            return Err(de::Error::custom("expected a ratio from 0.0000 to 1.0000"));
        }
        let fraction = value[2..].parse::<u16>().map_err(de::Error::custom)?;
        Ok(Self(fraction))
    }
}

impl Serialize for WeightRatio {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let text = if self.0 == 10_000 {
            "1.0000".to_owned()
        } else {
            format!("0.{:04}", self.0)
        };
        serializer.serialize_str(&text)
    }
}

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

#[cfg(test)]
mod tests {
    use super::WeightRatio;

    #[test]
    fn every_basis_point_has_an_exact_round_trip() {
        for points in 0..=10_000 {
            let ratio = WeightRatio(points);
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
