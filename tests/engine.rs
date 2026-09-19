use unicode_collision_workbench::engine::*;

fn cfg(table: UnicodeTable) -> RuleConfig {
    RuleConfig {
        unicode_table: Some(table),
        case_fold: true,
        normalization: Some(NormalizationForm::Nfc),
        strip_default_ignorable: false,
        script_policy: ScriptPolicy::Off,
        allowed_scripts: Default::default(),
    }
}

#[test]
fn reports_invalid_utf8_without_normalizing_bytes() {
    let analysis = analyze_records(
        &cfg(UnicodeTable::U16_0),
        &[EngineInput {
            record_id: "bad",
            source_version: 1,
            original_text: None,
            original_bytes: &[0x41, 0xff, 0x42],
            codepoint_source: false,
        }],
    )
    .unwrap();
    let record = &analysis.records[0];
    assert!(!record.eligible);
    assert_eq!(record.issues[0].kind, "invalid-utf8");
    assert_eq!(record.original_base64, "Qf9C");

    let wtf_surrogate = analyze_records(
        &cfg(UnicodeTable::U16_0),
        &[EngineInput {
            record_id: "wtf",
            source_version: 1,
            original_text: None,
            original_bytes: &[0xed, 0xa0, 0x80],
            codepoint_source: false,
        }],
    )
    .unwrap();
    assert!(!wtf_surrogate.records[0].eligible);
    assert_eq!(wtf_surrogate.records[0].issues[0].kind, "invalid-utf8");
}

#[test]
fn noncharacters_and_lone_surrogages_are_separate_issues() {
    let config = cfg(UnicodeTable::U16_0);
    let noncharacter = analyze_one(
        &config,
        EngineInput {
            record_id: "nc",
            source_version: 1,
            original_text: Some("x\u{ffff}"),
            original_bytes: "x\u{ffff}".as_bytes(),
            codepoint_source: false,
        },
    );
    assert!(noncharacter
        .issues
        .iter()
        .any(|issue| issue.kind == "noncharacter"));
    assert!(!noncharacter.eligible);
    let surrogate_payload = serde_json::to_vec(&[0xd800u32]).unwrap();
    let surrogate = analyze_one(
        &config,
        EngineInput {
            record_id: "surrogate",
            source_version: 1,
            original_text: None,
            original_bytes: &surrogate_payload,
            codepoint_source: true,
        },
    );
    assert!(surrogate
        .issues
        .iter()
        .any(|issue| issue.kind == "lone-surrogate"));
    assert!(!surrogate.eligible);
}

#[test]
fn visual_similarity_is_not_equivalence() {
    let analysis = analyze_records(
        &cfg(UnicodeTable::U16_0),
        &[
            EngineInput {
                record_id: "latin",
                source_version: 1,
                original_text: Some("l"),
                original_bytes: b"l",
                codepoint_source: false,
            },
            EngineInput {
                record_id: "ipa",
                source_version: 1,
                original_text: Some("ɩ"),
                original_bytes: "ɩ".as_bytes(),
                codepoint_source: false,
            },
        ],
    )
    .unwrap();
    assert_eq!(analysis.buckets.len(), 2);
    assert!(analysis
        .buckets
        .iter()
        .all(|bucket| bucket.record_ids.len() == 1));
}

#[test]
fn casefold_nfc_collision_stays_a_candidate() {
    let analysis = analyze_records(
        &cfg(UnicodeTable::U16_0),
        &[
            EngineInput {
                record_id: "a",
                source_version: 1,
                original_text: Some("Straße"),
                original_bytes: "Straße".as_bytes(),
                codepoint_source: false,
            },
            EngineInput {
                record_id: "b",
                source_version: 1,
                original_text: Some("STRASSE"),
                original_bytes: "STRASSE".as_bytes(),
                codepoint_source: false,
            },
        ],
    )
    .unwrap();
    assert_eq!(analysis.buckets.len(), 1);
    assert_eq!(analysis.buckets[0].record_ids, ["a", "b"]);
}

#[test]
fn staged_rule_versions_are_visible() {
    let analysis = analyze_text(&cfg(UnicodeTable::U15_1), "A").unwrap();
    assert!(analysis.records[0]
        .stages
        .iter()
        .all(|stage| stage.rule_version == "15.1.0"));
}
