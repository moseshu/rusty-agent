// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#![allow(clippy::expect_used)]

use ra_config::*;

#[test]
fn rejected_validator_does_not_replace_the_existing_contract() {
    let mut value = Constrained::allow_only(5);
    assert!(
        value
            .add_validator(|_| Err(ConstraintError::empty_field("policy")))
            .is_err()
    );
    assert_eq!(value.value(), 5);
    assert_eq!(value.set(5), Ok(()));
    assert!(value.set(6).is_err());
}

#[test]
fn validator_probe_does_not_apply_the_normalizer() -> ConstraintResult<()> {
    let mut value = Constrained::normalized(-1, |candidate| candidate.max(0))?;
    value.add_validator(|candidate| {
        if *candidate < 0 {
            Err(ConstraintError::empty_field("value"))
        } else {
            Ok(())
        }
    })?;
    assert!(value.can_set(&-5).is_err());
    value.set(-5)?;
    assert_eq!(value.value(), 0);
    Ok(())
}

#[test]
fn requirement_sources_flatten_deduplicate_and_keep_priority() {
    let high = RequirementSource::EnterpriseManaged {
        id: "req_1".into(),
        name: "Policy".into(),
    };
    let low = RequirementSource::MdmManagedPreferences {
        domain: "example.product".into(),
        key: "requirements_toml_base64".into(),
    };
    let composite = RequirementSource::composite([
        high.clone(),
        RequirementSource::Composite {
            sources: vec![low.clone(), high.clone()],
        },
        low.clone(),
    ]);
    assert_eq!(
        composite,
        RequirementSource::Composite {
            sources: vec![high.clone(), low]
        }
    );
    assert_eq!(RequirementSource::composite([]), RequirementSource::Unknown);
    assert_eq!(
        RequirementSource::composite([high.clone(), high]),
        RequirementSource::EnterpriseManaged {
            id: "req_1".into(),
            name: "Policy".into()
        }
    );
    let error = ConstraintError::InvalidValue {
        field_name: "approval_policy",
        candidate: "Never".into(),
        allowed: "[OnRequest]".into(),
        requirement_source: composite,
    };
    assert_eq!(
        error.to_string(),
        "invalid value for `approval_policy`: `Never` is not in the allowed set [OnRequest] (set by requirements layers: enterprise-managed requirements Policy (req_1), MDM example.product:requirements_toml_base64)"
    );
    let io_error = std::io::Error::from(error);
    assert_eq!(io_error.kind(), std::io::ErrorKind::InvalidInput);
}

fn invalid_value(candidate: impl Into<String>, allowed: impl Into<String>) -> ConstraintError {
    ConstraintError::InvalidValue {
        field_name: "<unknown>",
        candidate: candidate.into(),
        allowed: allowed.into(),
        requirement_source: RequirementSource::Unknown,
    }
}

#[test]
fn constrained_allow_any_accepts_any_value() {
    let mut constrained = Constrained::allow_any(/*initial_value*/ 5);
    constrained
        .set(/*value*/ -10)
        .expect("allow any accepts all values");
    assert_eq!(constrained.value(), -10);
}

#[test]
fn constrained_allow_any_default_uses_default_value() {
    let constrained = Constrained::<i32>::allow_any_from_default();
    assert_eq!(constrained.value(), 0);
}

#[test]
fn constrained_allow_only_rejects_different_values() {
    let mut constrained = Constrained::allow_only(/*only_value*/ 5);
    constrained
        .set(/*value*/ 5)
        .expect("allowed value should be accepted");

    let err = constrained
        .set(/*value*/ 6)
        .expect_err("different value should be rejected");
    assert_eq!(err, invalid_value("6", "[5]"));
    assert_eq!(constrained.value(), 5);
}

#[test]
fn constrained_normalizer_applies_on_init_and_set() -> ConstraintResult<()> {
    let mut constrained = Constrained::normalized(/*initial_value*/ -1, |value| value.max(0))?;
    assert_eq!(constrained.value(), 0);
    constrained.set(/*value*/ -5)?;
    assert_eq!(constrained.value(), 0);
    constrained.set(/*value*/ 10)?;
    assert_eq!(constrained.value(), 10);
    Ok(())
}

#[test]
fn constrained_add_validator_composes_with_existing_validator() -> ConstraintResult<()> {
    let mut constrained = Constrained::new(/*initial_value*/ 5, |value: &i32| {
        if *value >= 0 {
            Ok(())
        } else {
            Err(ConstraintError::empty_field("value"))
        }
    })?;
    constrained.add_validator(|value| {
        if *value <= 10 {
            Ok(())
        } else {
            Err(ConstraintError::empty_field("value"))
        }
    })?;

    assert_eq!(constrained.can_set(&7), Ok(()));
    assert_eq!(
        constrained.can_set(&11),
        Err(ConstraintError::empty_field("value"))
    );
    assert_eq!(
        constrained.can_set(&-1),
        Err(ConstraintError::empty_field("value"))
    );

    Ok(())
}

#[test]
fn constrained_new_rejects_invalid_initial_value() {
    let result = Constrained::new(/*initial_value*/ 0, |value| {
        if *value > 0 {
            Ok(())
        } else {
            Err(invalid_value(value.to_string(), "positive values"))
        }
    });

    assert_eq!(result, Err(invalid_value("0", "positive values")));
}

#[test]
fn constrained_set_rejects_invalid_value_and_leaves_previous() {
    let mut constrained = Constrained::new(/*initial_value*/ 1, |value| {
        if *value > 0 {
            Ok(())
        } else {
            Err(invalid_value(value.to_string(), "positive values"))
        }
    })
    .expect("initial value should be accepted");

    let err = constrained
        .set(/*value*/ -5)
        .expect_err("negative values should be rejected");
    assert_eq!(err, invalid_value("-5", "positive values"));
    assert_eq!(constrained.value(), 1);
}

#[test]
fn constrained_can_set_allows_probe_without_setting() {
    let constrained = Constrained::new(/*initial_value*/ 1, |value| {
        if *value > 0 {
            Ok(())
        } else {
            Err(invalid_value(value.to_string(), "positive values"))
        }
    })
    .expect("initial value should be accepted");

    constrained
        .can_set(&2)
        .expect("can_set should accept positive value");
    let err = constrained
        .can_set(&-1)
        .expect_err("can_set should reject negative value");
    assert_eq!(err, invalid_value("-1", "positive values"));
    assert_eq!(constrained.value(), 1);
}
