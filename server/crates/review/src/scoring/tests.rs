use super::*;

fn ratio(value: u16) -> WeightRatio {
    WeightRatio::from_basis_points(value).unwrap()
}
fn score(value: u16) -> Score {
    Score::from_hundredths(value).unwrap()
}

fn platform(rows: &[(&str, u16, u16)]) -> PlatformDimensions {
    PlatformDimensions::new(
        rows.iter()
            .map(|(key, weight, ai)| DimensionConfig {
                dimension_key: (*key).to_owned(),
                title: format!("维度 {key}"),
                weight_ratio: ratio(*weight),
                ai_weight_ratio: ratio(*ai),
                human_weight_ratio: ratio(10_000 - ai),
            })
            .collect(),
    )
    .unwrap()
}

fn rules(platform: PlatformDimensions, items: &[(&str, &[(&str, u16)])]) -> ScoringRules {
    ScoringRules::new(
        platform,
        items
            .iter()
            .map(|(key, items)| DimensionIndicators {
                dimension_key: (*key).to_owned(),
                indicators: items
                    .iter()
                    .map(|(key, weight)| IndicatorWeight {
                        indicator_key: (*key).to_owned(),
                        weight_ratio: ratio(*weight),
                    })
                    .collect(),
            })
            .collect(),
    )
    .unwrap()
}

fn ai(dimension: &str, indicator: &str, value: u16) -> AiIndicatorResult {
    AiIndicatorResult {
        dimension_key: dimension.to_owned(),
        indicator_key: indicator.to_owned(),
        result: IndicatorResult::Scored(score(value)),
    }
}

fn human(dimension: &str, value: u16) -> HumanDimensionResult {
    HumanDimensionResult {
        dimension_key: dimension.to_owned(),
        score: score(value),
    }
}

#[test]
fn score_json_is_exact_including_exponents_and_redundant_zeroes() {
    for (text, expected) in [
        ("0", 0),
        ("-0.00", 0),
        ("0.01", 1),
        ("100.00", 10_000),
        ("1e2", 10_000),
        ("7500e-2", 7500),
        ("7.5000e1", 7500),
    ] {
        assert_eq!(
            serde_json::from_str::<Score>(text).unwrap(),
            score(expected)
        );
    }
    for value in 0..=10_000 {
        let original = score(value);
        let json = serde_json::to_string(&original).unwrap();
        assert_eq!(serde_json::from_str::<Score>(&json).unwrap(), original);
        assert!(!json.starts_with('"'));
        assert_eq!(
            serde_json::from_value::<Score>(serde_json::to_value(original).unwrap()).unwrap(),
            original
        );
    }
}

#[test]
fn invalid_scores_cannot_be_rounded_into_valid_inputs() {
    for text in [
        "null",
        "true",
        "[]",
        "{}",
        "\"75\"",
        "-0.01",
        "100.01",
        "1.005",
        "1.0000000000000000001",
        "100.00000000000000001",
        "1e999999999",
        "1e-999999999",
        "{\"$serde_json::private::Number\":\"75.00\"}",
        "{\"$serde_json::private::RawValue\":\"75.00\"}",
    ] {
        assert!(serde_json::from_str::<Score>(text).is_err(), "{text}");
    }
    assert!(Score::from_hundredths(10_001).is_err());
    assert!(WeightRatio::from_basis_points(10_001).is_err());
}

#[test]
fn platform_dimensions_preserve_any_count_order_names_and_ratios() {
    for count in [1, 3, 5, 6, 7, 19] {
        let input: Vec<_> = (0..count)
            .map(|index| DimensionConfig {
                dimension_key: format!("custom_{}", count - index),
                title: format!("自定名称 {index}"),
                weight_ratio: ratio(if index == 0 { 10_000 - (count - 1) } else { 1 }),
                ai_weight_ratio: ratio(3725),
                human_weight_ratio: ratio(6275),
            })
            .collect();
        let config = PlatformDimensions::new(input.clone()).unwrap();
        assert_eq!(config.dimensions(), input);
        assert!(config.requires_ai());
        assert_eq!(
            config.required_human_dimensions().count(),
            usize::from(count)
        );
    }
}

#[test]
fn invalid_configuration_returns_specific_paths_without_repairing_values() {
    let mut dimensions = platform(&[("a", 5000, 4000), ("b", 5000, 4000)])
        .dimensions()
        .to_vec();
    dimensions[1].dimension_key = "a".to_owned();
    dimensions[1].human_weight_ratio = ratio(5000);
    dimensions[0].weight_ratio = ratio(4000);
    let issues = PlatformDimensions::new(dimensions).unwrap_err();
    assert!(issues.contains(&ValidationIssue::new(
        ValidationCode::DuplicateDimensionKey,
        "dimensions.1.dimension_key"
    )));
    assert!(issues.contains(&ValidationIssue::new(
        ValidationCode::InvalidChannelWeights,
        "dimensions.1"
    )));
    assert!(issues.contains(&ValidationIssue::new(
        ValidationCode::InvalidDimensionWeights,
        "dimensions"
    )));
    assert!(PlatformDimensions::new(vec![]).is_err());
}

#[test]
fn names_are_validated_without_language_restrictions_or_silent_trimming() {
    for (key, title, valid) in [
        ("a", "English and 中文", true),
        ("a", "  保留空格  ", true),
        ("a", " \n\t", false),
        ("a", "bad\u{7f}", false),
        ("a\n", "标题", false),
        ("CamelCase", "标题", false),
    ] {
        let mut dimensions = platform(&[("a", 10_000, 10_000)]).dimensions().to_vec();
        dimensions[0].dimension_key = key.to_owned();
        dimensions[0].title = title.to_owned();
        let result = PlatformDimensions::new(dimensions);
        assert_eq!(result.is_ok(), valid, "{key} {title}");
        if let Ok(config) = result {
            assert_eq!(config.dimensions()[0].title, title);
        }
    }
}

#[test]
fn indicator_definitions_must_match_platform_dimensions_and_exact_weights() {
    let config = platform(&[("a", 10_000, 10_000)]);
    assert!(ScoringRules::new(config.clone(), vec![]).is_err());
    for items in [
        vec![],
        vec![IndicatorWeight {
            indicator_key: "i".into(),
            weight_ratio: ratio(9000),
        }],
        vec![
            IndicatorWeight {
                indicator_key: "i".into(),
                weight_ratio: ratio(5000)
            };
            2
        ],
    ] {
        assert!(
            ScoringRules::new(
                config.clone(),
                vec![DimensionIndicators {
                    dimension_key: "a".into(),
                    indicators: items
                }]
            )
            .is_err()
        );
    }
    let extra = DimensionIndicators {
        dimension_key: "made_up".into(),
        indicators: vec![IndicatorWeight {
            indicator_key: "i".into(),
            weight_ratio: ratio(10_000),
        }],
    };
    assert!(
        ScoringRules::new(config, vec![extra])
            .unwrap_err()
            .iter()
            .any(|issue| issue.code == ValidationCode::UnknownDimensionKey)
    );
}

#[test]
fn zero_is_counted_but_missing_indicator_never_becomes_zero_or_changes_denominator() {
    let rules = rules(
        platform(&[("a", 10_000, 10_000)]),
        &[("a", &[("first", 5000), ("second", 5000)])],
    );
    let complete = rules
        .calculate(&[ai("a", "first", 8000), ai("a", "second", 0)], &[])
        .unwrap();
    assert_eq!(complete.ai_score, Some(score(4000)));
    assert_eq!(complete.final_score, Some(score(4000)));
    let partial = rules.calculate(&[ai("a", "first", 8000)], &[]).unwrap();
    assert_eq!(partial.ai_score, None);
    assert_eq!(partial.final_score, None);
    assert_eq!(partial.dimensions[0].ai_score, None);
    assert_eq!(
        partial.dimensions[0].indicators[0].result,
        Some(IndicatorResult::Scored(score(8000)))
    );
    assert_eq!(partial.score_status, ScoreStatus::Partial);
    assert_eq!(partial.summary_status, SummaryStatus::Partial);
    assert_eq!(
        partial.missing_scores,
        vec![missing_ai(
            "a",
            "second",
            MissingCategory::Pending,
            "SCORE_PENDING"
        )]
    );
}

#[test]
fn prd_channel_total_is_distinct_from_final_total() {
    let rules = rules(
        platform(&[("a", 5000, 4000), ("b", 5000, 10_000)]),
        &[("b", &[("i", 10_000)]), ("a", &[("i", 10_000)])],
    );
    let result = rules
        .calculate(
            &[ai("b", "i", 6000), ai("a", "i", 8000)],
            &[human("a", 9000)],
        )
        .unwrap();
    assert_eq!(result.dimensions[0].configuration.dimension_key, "a");
    assert_eq!(result.ai_score, Some(score(6571)));
    assert_eq!(result.final_score, Some(score(7300)));
    assert_eq!(result.dimensions[0].final_score, Some(score(8600)));
    assert_eq!(result.score_status, ScoreStatus::Complete);
    assert_eq!(result.summary_status, SummaryStatus::Complete);
    assert!(result.missing_scores.is_empty());
}

#[test]
fn arbitrary_channel_ratios_use_half_up_and_do_not_change_ai_result() {
    for ai_ratio in 0..=10_000 {
        let rules = rules(
            platform(&[("custom", 10_000, ai_ratio)]),
            &[("custom", &[("i", 10_000)])],
        );
        let ai_results = if ai_ratio > 0 {
            vec![ai("custom", "i", 8000)]
        } else {
            vec![]
        };
        let human_results = if ai_ratio < 10_000 {
            vec![human("custom", 9000)]
        } else {
            vec![]
        };
        let result = rules.calculate(&ai_results, &human_results).unwrap();
        let expected =
            (80_u64 * u64::from(ai_ratio) + 90 * u64::from(10_000 - ai_ratio) + 50) / 100;
        assert_eq!(
            result.final_score,
            Some(score(expected as u16)),
            "{ai_ratio}"
        );
        assert_eq!(result.ai_score, (ai_ratio > 0).then_some(score(8000)));
        if ai_ratio == 3725 {
            assert_eq!(result.final_score.unwrap().to_string(), "86.28");
        }
    }
}

#[test]
fn all_human_needs_no_ai_and_technical_name_does_not_force_ai() {
    let rules = rules(
        platform(&[("technical_implementation", 3000, 0), ("b", 7000, 0)]),
        &[
            ("technical_implementation", &[("i", 10_000)]),
            ("b", &[("i", 10_000)]),
        ],
    );
    assert!(!rules.configuration().requires_ai());
    let result = rules
        .calculate(
            &[],
            &[human("technical_implementation", 8000), human("b", 9000)],
        )
        .unwrap();
    assert_eq!(result.ai_score, None);
    assert_eq!(result.score_status, ScoreStatus::NotRequired);
    assert_eq!(result.final_score, Some(score(8700)));
    assert!(result.missing_scores.is_empty());
    assert!(result.dimensions[0].indicators[0].result.is_none());
}

#[test]
fn missing_optional_channels_dimensions_and_indicators_do_not_block() {
    let rules = rules(
        platform(&[("required", 10_000, 10_000), ("optional", 0, 5000)]),
        &[
            ("required", &[("i", 10_000), ("optional_item", 0)]),
            ("optional", &[("i", 10_000)]),
        ],
    );
    let result = rules.calculate(&[ai("required", "i", 0)], &[]).unwrap();
    assert_eq!(result.final_score, Some(score(0)));
    assert_eq!(result.ai_score, Some(score(0)));
    assert!(result.missing_scores.is_empty());
    assert_eq!(result.dimensions[1].final_score, None);
    let with_optional = rules
        .calculate(
            &[ai("required", "i", 0), ai("optional", "i", 7000)],
            &[human("optional", 9000)],
        )
        .unwrap();
    assert_eq!(with_optional.final_score, Some(score(0)));
    assert_eq!(
        with_optional.dimensions[1].score_status,
        ScoreStatus::NotRequired
    );
    assert_eq!(with_optional.dimensions[1].ai_score, None);
    assert_eq!(
        with_optional.dimensions[1].indicators[0].result,
        Some(IndicatorResult::Scored(score(7000)))
    );
    assert_eq!(with_optional.dimensions[1].final_score, Some(score(8000)));
}

#[test]
fn rounding_happens_after_all_three_weight_levels() {
    let rules = rules(
        platform(&[("a", 5000, 10_000), ("b", 5000, 10_000)]),
        &[
            ("a", &[("first", 5000), ("second", 5000)]),
            ("b", &[("first", 6000), ("second", 4000)]),
        ],
    );
    let result = rules
        .calculate(
            &[
                ai("a", "first", 7999),
                ai("a", "second", 8000),
                ai("b", "first", 7999),
                ai("b", "second", 8000),
            ],
            &[],
        )
        .unwrap();
    assert_eq!(result.dimensions[0].ai_score, Some(score(8000)));
    assert_eq!(result.dimensions[1].ai_score, Some(score(7999)));
    // 精确值为 79.9945；使用展示维度分再聚合会错误得到 80.00。
    assert_eq!(result.ai_score, Some(score(7999)));
    assert_eq!(result.final_score, Some(score(7999)));
    let rules = super::tests::rules(
        platform(&[("a", 10_000, 5000)]),
        &[("a", &[("first", 5000), ("second", 5000)])],
    );
    let result = rules
        .calculate(
            &[ai("a", "first", 7999), ai("a", "second", 8000)],
            &[human("a", 7999)],
        )
        .unwrap();
    assert_eq!(result.ai_score, Some(score(8000)));
    assert_eq!(result.final_score, Some(score(7999)));
}

#[test]
fn unavailable_reason_is_preserved_and_does_not_hide_other_dimensions() {
    let rules = rules(
        platform(&[("a", 5000, 10_000), ("b", 5000, 10_000)]),
        &[("a", &[("i", 10_000)]), ("b", &[("i", 10_000)])],
    );
    let failed = AiIndicatorResult {
        dimension_key: "a".into(),
        indicator_key: "i".into(),
        result: IndicatorResult::Unavailable {
            category: MissingCategory::Provider,
            reason_code: "MODEL_OUTPUT_INVALID".into(),
        },
    };
    let result = rules
        .calculate(&[failed.clone(), ai("b", "i", 7500)], &[])
        .unwrap();
    assert_eq!(result.dimensions[1].ai_score, Some(score(7500)));
    assert_eq!(result.score_status, ScoreStatus::Partial);
    assert_eq!(result.final_score, None);
    assert_eq!(
        result.missing_scores,
        vec![missing_ai(
            "a",
            "i",
            MissingCategory::Provider,
            "MODEL_OUTPUT_INVALID"
        )]
    );
    let single = super::tests::rules(
        platform(&[("a", 10_000, 10_000)]),
        &[("a", &[("i", 10_000)])],
    );
    assert_eq!(
        single.calculate(&[failed], &[]).unwrap().score_status,
        ScoreStatus::Unavailable
    );
    assert_eq!(
        single.calculate(&[], &[]).unwrap().score_status,
        ScoreStatus::Pending
    );
}

#[test]
fn human_revision_changes_only_summary_and_inputs_are_reusable() {
    let rules = rules(platform(&[("a", 10_000, 4000)]), &[("a", &[("i", 10_000)])]);
    let results = [ai("a", "i", 8000)];
    let first = rules.calculate(&results, &[human("a", 9000)]).unwrap();
    let next = rules.calculate(&results, &[human("a", 7000)]).unwrap();
    assert_eq!(first.ai_score, next.ai_score);
    assert_eq!(first.final_score, Some(score(8600)));
    assert_eq!(next.final_score, Some(score(7400)));
    assert_eq!(results[0].result, IndicatorResult::Scored(score(8000)));
    assert_eq!(
        rules
            .calculate(&[], &[human("a", 9000)])
            .unwrap()
            .summary_status,
        SummaryStatus::Partial
    );
}

#[test]
fn duplicate_or_unknown_results_are_rejected_instead_of_last_write_wins() {
    let rules = rules(platform(&[("a", 10_000, 4000)]), &[("a", &[("i", 10_000)])]);
    for inputs in [
        vec![ai("a", "i", 5000), ai("a", "i", 9000)],
        vec![ai("unknown", "i", 5000)],
        vec![ai("a", "unknown", 5000)],
    ] {
        assert!(rules.calculate(&inputs, &[]).is_err());
    }
    assert!(
        rules
            .calculate(&[], &[human("a", 5000), human("a", 9000)])
            .is_err()
    );
    assert!(rules.calculate(&[], &[human("unknown", 9000)]).is_err());
    let pure_ai = super::tests::rules(
        platform(&[("a", 10_000, 10_000)]),
        &[("a", &[("i", 10_000)])],
    );
    assert_eq!(
        pure_ai.calculate(&[], &[human("a", 9000)]).unwrap_err()[0].code,
        ValidationCode::HumanDimensionNotAllowed
    );
}

#[test]
fn validation_paths_follow_indicator_input_order() {
    let config = platform(&[("a", 5000, 10_000), ("b", 5000, 10_000)]);
    let definitions = vec![
        DimensionIndicators {
            dimension_key: "b".into(),
            indicators: vec![IndicatorWeight {
                indicator_key: "i".into(),
                weight_ratio: ratio(10_000),
            }],
        },
        DimensionIndicators {
            dimension_key: "a".into(),
            indicators: vec![IndicatorWeight {
                indicator_key: "i".into(),
                weight_ratio: ratio(9000),
            }],
        },
    ];
    assert_eq!(
        ScoringRules::new(config, definitions).unwrap_err(),
        vec![ValidationIssue::new(
            ValidationCode::InvalidIndicatorWeights,
            "dimensions.1.indicators"
        )]
    );
}

#[test]
fn decisions_use_explicit_thresholds_and_complete_rounded_scores_only() {
    let thresholds = DecisionThresholds::new(score(8000), score(5000)).unwrap();
    for (value, expected) in [
        (0, Decision::Fail),
        (4999, Decision::Fail),
        (5000, Decision::ConditionalPass),
        (7999, Decision::ConditionalPass),
        (8000, Decision::Pass),
        (10_000, Decision::Pass),
    ] {
        assert_eq!(thresholds.classify(Some(score(value))), Some(expected));
    }
    assert_eq!(thresholds.classify(None), None);
    assert!(DecisionThresholds::new(score(5000), score(5000)).is_err());
    assert!(DecisionThresholds::new(score(4000), score(5000)).is_err());
}
