//! 比例按万分位、分数按百分之一分保存，不经过二进制浮点运算。

use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

pub(super) const BASIS: u128 = 10_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidNumber;

impl fmt::Display for InvalidNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("数值范围或精度不符合要求")
    }
}

impl std::error::Error for InvalidNumber {}

/// 平台比例，JSON 使用 0.0000..1.0000 四位小数字符串。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WeightRatio(u16);

impl WeightRatio {
    pub fn from_basis_points(value: u16) -> Result<Self, InvalidNumber> {
        (value <= 10_000)
            .then_some(Self(value))
            .ok_or(InvalidNumber)
    }

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
            return Err(de::Error::custom(InvalidNumber));
        }
        Ok(Self(value[2..].parse().map_err(de::Error::custom)?))
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

/// 有效百分制分数。0 是有效分；缺分必须由所属结果的独立状态表示。
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Score(u16);

impl Score {
    pub fn from_hundredths(value: u16) -> Result<Self, InvalidNumber> {
        (value <= 10_000)
            .then_some(Self(value))
            .ok_or(InvalidNumber)
    }

    pub fn hundredths(self) -> u16 {
        self.0
    }

    // 分子单位为百分之一分；只对已验证权重形成的非负有界结果舍入。
    pub(super) fn round_half_up(numerator: u128, denominator: u128) -> Self {
        let rounded = (numerator + denominator / 2) / denominator;
        debug_assert!(rounded <= 10_000 && denominator > 0);
        Self(rounded as u16)
    }
}

impl fmt::Display for Score {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{:02}", self.0 / 100, self.0 % 100)
    }
}

impl<'de> Deserialize<'de> for Score {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // arbitrary_precision 保留原始 JSON 十进制文本，避免先转 f64 后掩盖超精度输入。
        let raw = Box::<serde_json::value::RawValue>::deserialize(deserializer)?;
        let number = serde_json::Number::from_str(raw.get()).map_err(de::Error::custom)?;
        let text = number.to_string();
        parse_score(&text).map_err(de::Error::custom)
    }
}

impl Serialize for Score {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serde_json::Number::from_str(&self.to_string())
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }
}

fn parse_score(text: &str) -> Result<Score, InvalidNumber> {
    let (mantissa, exponent) = text.split_once(['e', 'E']).unwrap_or((text, "0"));
    let negative = mantissa.starts_with('-');
    let mantissa = mantissa.strip_prefix('-').unwrap_or(mantissa);
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits = format!("{whole}{fraction}");
    let significant = digits.trim_start_matches('0');
    if significant.is_empty() {
        return Ok(Score(0));
    }
    if negative {
        return Err(InvalidNumber);
    }
    let trimmed = significant.trim_end_matches('0');
    let exponent: i64 = exponent.parse().map_err(|_| InvalidNumber)?;
    let shift = exponent
        .checked_sub(fraction.len() as i64)
        .and_then(|value| value.checked_add((significant.len() - trimmed.len()) as i64 + 2))
        .ok_or(InvalidNumber)?;
    if !(0..=4).contains(&shift) || trimmed.len() + shift as usize > 5 {
        return Err(InvalidNumber);
    }
    let value = trimmed.parse::<u32>().map_err(|_| InvalidNumber)? * 10_u32.pow(shift as u32);
    if value > 10_000 {
        return Err(InvalidNumber);
    }
    Ok(Score(value as u16))
}
