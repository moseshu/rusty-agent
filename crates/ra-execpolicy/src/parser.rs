//! Parsing rules files: Codex's `execpolicy/src/parser.rs`.
//!
//! A file is evaluated as Starlark in the extended dialect with f-strings enabled, with the
//! standard globals and the three policy builtins. Every file adds to one builder, so later files
//! add to the rules of earlier ones; the `match` and `not_match` examples of a file are checked
//! once the file has been evaluated, against the rules of their own `prefix_rule` call and the host
//! executables known by then.

use std::{cell::RefCell, collections::HashMap, path::Path, sync::Arc};

use multimap::MultiMap;
use starlark::{
    codemap::FileSpan,
    environment::{GlobalsBuilder, Module},
    eval::Evaluator,
    starlark_module,
    syntax::{AstModule, Dialect},
    values::{
        Value,
        list::{ListRef, UnpackList},
        none::NoneType,
    },
};

use crate::{
    absolute_path::AbsolutePathBuf,
    decision::Decision,
    error::{Error, ErrorLocation, Result, TextPosition, TextRange},
    executable_name::{executable_lookup_key, executable_path_lookup_key},
    rule::{
        NetworkRule, NetworkRuleProtocol, PatternToken, PrefixPattern, PrefixRule, RuleRef,
        validate_match_examples, validate_not_match_examples,
    },
};

/// Builds a [`Policy`](crate::Policy) from rules files, parsed in order.
///
/// # How the builtins reach the builder
///
/// Codex hands its builder to the builtins as the evaluator's `extra` value, which requires the
/// builder to implement Starlark's `ProvidesStaticType` through a derive that expands to an
/// `unsafe impl`. This workspace forbids unsafe code, so the builder is lent to the builtins
/// through a thread-local for the duration of one evaluation instead: evaluation is synchronous
/// on the calling thread, and the builder is put back however it ends, with whatever the file
/// added before an error, as Codex's in-place builder keeps it.
pub struct PolicyParser {
    builder: PolicyBuilder,
}

thread_local! {
    static ACTIVE_BUILDER: RefCell<Option<PolicyBuilder>> = const { RefCell::new(None) };
}

/// Lends a builder to the policy builtins and takes it back when dropped.
struct LentBuilder<'a> {
    owner: &'a mut PolicyBuilder,
}

impl<'a> LentBuilder<'a> {
    fn lend(owner: &'a mut PolicyBuilder) -> Self {
        let builder = std::mem::replace(owner, PolicyBuilder::new());
        ACTIVE_BUILDER.with(|cell| *cell.borrow_mut() = Some(builder));
        Self { owner }
    }
}

impl Drop for LentBuilder<'_> {
    fn drop(&mut self) {
        if let Some(builder) = ACTIVE_BUILDER.with(|cell| cell.borrow_mut().take()) {
            *self.owner = builder;
        }
    }
}

impl Default for PolicyParser {
    fn default() -> Self {
        Self::new()
    }
}

impl PolicyParser {
    /// A parser with no rules yet.
    #[must_use]
    pub fn new() -> Self {
        Self {
            builder: PolicyBuilder::new(),
        }
    }

    /// Parses a policy, tagging parser errors with `policy_identifier` so failures include the
    /// identifier alongside line numbers.
    pub fn parse(&mut self, policy_identifier: &str, policy_file_contents: &str) -> Result<()> {
        let pending_validation_count = self.builder.pending_example_validations.len();
        let mut dialect = Dialect::Extended.clone();
        dialect.enable_f_strings = true;
        let ast = AstModule::parse(
            policy_identifier,
            policy_file_contents.to_string(),
            &dialect,
        )
        .map_err(Error::Starlark)?;
        let globals = GlobalsBuilder::standard().with(policy_builtins).build();
        {
            let _lent = LentBuilder::lend(&mut self.builder);
            Module::with_temp_heap(|module| {
                let mut eval = Evaluator::new(&module);
                eval.eval_module(ast, &globals)
                    .map(|_| ())
                    .map_err(Error::Starlark)
            })?;
        }
        self.builder
            .validate_pending_examples_from(pending_validation_count)?;
        Ok(())
    }

    /// The policy every parsed file defines.
    #[must_use]
    pub fn build(self) -> crate::policy::Policy {
        self.builder.build()
    }
}

#[derive(Debug)]
struct PolicyBuilder {
    rules_by_program: MultiMap<String, RuleRef>,
    network_rules: Vec<NetworkRule>,
    host_executables_by_name: HashMap<String, Arc<[AbsolutePathBuf]>>,
    pending_example_validations: Vec<PendingExampleValidation>,
}

impl PolicyBuilder {
    fn new() -> Self {
        Self {
            rules_by_program: MultiMap::new(),
            network_rules: Vec::new(),
            host_executables_by_name: HashMap::new(),
            pending_example_validations: Vec::new(),
        }
    }

    fn add_rule(&mut self, rule: RuleRef) {
        self.rules_by_program
            .insert(rule.program().to_string(), rule);
    }

    fn add_network_rule(&mut self, rule: NetworkRule) {
        self.network_rules.push(rule);
    }

    fn add_host_executable(&mut self, name: String, paths: Vec<AbsolutePathBuf>) {
        self.host_executables_by_name.insert(name, paths.into());
    }

    fn add_pending_example_validation(
        &mut self,
        rules: Vec<RuleRef>,
        matches: Vec<Vec<String>>,
        not_matches: Vec<Vec<String>>,
        location: Option<ErrorLocation>,
    ) {
        self.pending_example_validations
            .push(PendingExampleValidation {
                rules,
                matches,
                not_matches,
                location,
            });
    }

    fn validate_pending_examples_from(&self, start: usize) -> Result<()> {
        for validation in &self.pending_example_validations[start..] {
            let mut rules_by_program = MultiMap::new();
            for rule in &validation.rules {
                rules_by_program.insert(rule.program().to_string(), rule.clone());
            }

            let policy = crate::policy::Policy::from_parts(
                rules_by_program,
                Vec::new(),
                self.host_executables_by_name.clone(),
            );
            validate_not_match_examples(&policy, &validation.rules, &validation.not_matches)
                .map_err(|error| attach_validation_location(error, validation.location.clone()))?;
            validate_match_examples(&policy, &validation.rules, &validation.matches)
                .map_err(|error| attach_validation_location(error, validation.location.clone()))?;
        }

        Ok(())
    }

    fn build(self) -> crate::policy::Policy {
        crate::policy::Policy::from_parts(
            self.rules_by_program,
            self.network_rules,
            self.host_executables_by_name,
        )
    }
}

#[derive(Debug)]
struct PendingExampleValidation {
    rules: Vec<RuleRef>,
    matches: Vec<Vec<String>>,
    not_matches: Vec<Vec<String>>,
    location: Option<ErrorLocation>,
}

fn parse_pattern(pattern: UnpackList<Value<'_>>) -> Result<Vec<PatternToken>> {
    let tokens: Vec<PatternToken> = pattern
        .items
        .into_iter()
        .map(parse_pattern_token)
        .collect::<Result<_>>()?;
    if tokens.is_empty() {
        Err(Error::InvalidPattern("pattern cannot be empty".to_string()))
    } else {
        Ok(tokens)
    }
}

fn parse_pattern_token(value: Value<'_>) -> Result<PatternToken> {
    if let Some(s) = value.unpack_str() {
        Ok(PatternToken::Single(s.to_string()))
    } else if let Some(list) = ListRef::from_value(value) {
        let tokens: Vec<String> = list
            .content()
            .iter()
            .map(|value| {
                value
                    .unpack_str()
                    .ok_or_else(|| {
                        Error::InvalidPattern(format!(
                            "pattern alternative must be a string (got {})",
                            value.get_type()
                        ))
                    })
                    .map(str::to_string)
            })
            .collect::<Result<_>>()?;

        match tokens.as_slice() {
            [] => Err(Error::InvalidPattern(
                "pattern alternatives cannot be empty".to_string(),
            )),
            [single] => Ok(PatternToken::Single(single.clone())),
            _ => Ok(PatternToken::Alts(tokens)),
        }
    } else {
        Err(Error::InvalidPattern(format!(
            "pattern element must be a string or list of strings (got {})",
            value.get_type()
        )))
    }
}

fn parse_examples(examples: UnpackList<Value<'_>>) -> Result<Vec<Vec<String>>> {
    examples.items.into_iter().map(parse_example).collect()
}

fn parse_literal_absolute_path(raw: &str) -> Result<AbsolutePathBuf> {
    if !Path::new(raw).is_absolute() {
        return Err(Error::InvalidRule(format!(
            "host_executable paths must be absolute (got {raw})"
        )));
    }

    AbsolutePathBuf::try_from(raw.to_string())
        .map_err(|error| Error::InvalidRule(format!("invalid absolute path `{raw}`: {error}")))
}

fn validate_host_executable_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(Error::InvalidRule(
            "host_executable name cannot be empty".to_string(),
        ));
    }

    let path = Path::new(name);
    if path.components().count() != 1
        || path.file_name().and_then(|value| value.to_str()) != Some(name)
    {
        return Err(Error::InvalidRule(format!(
            "host_executable name must be a bare executable name (got {name})"
        )));
    }

    Ok(())
}

fn parse_network_rule_decision(raw: &str) -> Result<Decision> {
    match raw {
        "deny" => Ok(Decision::Forbidden),
        other => Decision::parse(other),
    }
}

fn error_location_from_file_span(span: &FileSpan) -> ErrorLocation {
    let resolved = span.resolve_span();
    ErrorLocation {
        path: span.filename().to_string(),
        range: TextRange {
            start: TextPosition {
                line: resolved.begin.line + 1,
                column: resolved.begin.column + 1,
            },
            end: TextPosition {
                line: resolved.end.line + 1,
                column: resolved.end.column + 1,
            },
        },
    }
}

fn attach_validation_location(error: Error, location: Option<ErrorLocation>) -> Error {
    match location {
        Some(location) => error.with_location(location),
        None => error,
    }
}

fn parse_example(value: Value<'_>) -> Result<Vec<String>> {
    if let Some(raw) = value.unpack_str() {
        parse_string_example(raw)
    } else if let Some(list) = ListRef::from_value(value) {
        parse_list_example(list)
    } else {
        Err(Error::InvalidExample(format!(
            "example must be a string or list of strings (got {})",
            value.get_type()
        )))
    }
}

fn parse_string_example(raw: &str) -> Result<Vec<String>> {
    let tokens = shlex::split(raw).ok_or_else(|| {
        Error::InvalidExample("example string has invalid shell syntax".to_string())
    })?;

    if tokens.is_empty() {
        Err(Error::InvalidExample(
            "example cannot be an empty string".to_string(),
        ))
    } else {
        Ok(tokens)
    }
}

fn parse_list_example(list: &ListRef) -> Result<Vec<String>> {
    let tokens: Vec<String> = list
        .content()
        .iter()
        .map(|value| {
            value
                .unpack_str()
                .ok_or_else(|| {
                    Error::InvalidExample(format!(
                        "example tokens must be strings (got {})",
                        value.get_type()
                    ))
                })
                .map(str::to_string)
        })
        .collect::<Result<_>>()?;

    if tokens.is_empty() {
        Err(Error::InvalidExample(
            "example cannot be an empty list".to_string(),
        ))
    } else {
        Ok(tokens)
    }
}

fn with_policy_builder<R>(apply: impl FnOnce(&mut PolicyBuilder) -> R) -> anyhow::Result<R> {
    ACTIVE_BUILDER.with(|cell| {
        cell.borrow_mut()
            .as_mut()
            .map(apply)
            .ok_or_else(|| anyhow::anyhow!("policy builtins run only while a rules file is parsed"))
    })
}

#[starlark_module]
fn policy_builtins(builder: &mut GlobalsBuilder) {
    fn prefix_rule<'v>(
        pattern: UnpackList<Value<'v>>,
        decision: Option<&'v str>,
        r#match: Option<UnpackList<Value<'v>>>,
        not_match: Option<UnpackList<Value<'v>>>,
        justification: Option<&'v str>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let decision = match decision {
            Some(raw) => Decision::parse(raw)?,
            None => Decision::Allow,
        };

        let justification = match justification {
            Some(raw) if raw.trim().is_empty() => {
                return Err(Error::InvalidRule("justification cannot be empty".to_string()).into());
            }
            Some(raw) => Some(raw.to_string()),
            None => None,
        };

        let pattern_tokens = parse_pattern(pattern)?;

        let matches: Vec<Vec<String>> =
            r#match.map(parse_examples).transpose()?.unwrap_or_default();
        let not_matches: Vec<Vec<String>> = not_match
            .map(parse_examples)
            .transpose()?
            .unwrap_or_default();
        let location = eval
            .call_stack_top_location()
            .map(|span| error_location_from_file_span(&span));

        let (first_token, remaining_tokens) = pattern_tokens
            .split_first()
            .ok_or_else(|| Error::InvalidPattern("pattern cannot be empty".to_string()))?;

        let rest: Arc<[PatternToken]> = remaining_tokens.to_vec().into();

        let rules: Vec<RuleRef> = first_token
            .alternatives()
            .iter()
            .map(|head| {
                Arc::new(PrefixRule {
                    pattern: PrefixPattern {
                        first: Arc::from(head.as_str()),
                        rest: rest.clone(),
                    },
                    decision,
                    justification: justification.clone(),
                }) as RuleRef
            })
            .collect();

        with_policy_builder(|builder| {
            builder.add_pending_example_validation(rules.clone(), matches, not_matches, location);
            for rule in rules {
                builder.add_rule(rule);
            }
        })?;
        Ok(NoneType)
    }

    fn network_rule<'v>(
        host: &'v str,
        protocol: &'v str,
        decision: &'v str,
        justification: Option<&'v str>,
    ) -> anyhow::Result<NoneType> {
        let protocol = NetworkRuleProtocol::parse(protocol)?;
        let decision = parse_network_rule_decision(decision)?;
        let justification = match justification {
            Some(raw) if raw.trim().is_empty() => {
                return Err(Error::InvalidRule("justification cannot be empty".to_string()).into());
            }
            Some(raw) => Some(raw.to_string()),
            None => None,
        };

        let rule = NetworkRule {
            host: crate::rule::normalize_network_rule_host(host)?,
            protocol,
            decision,
            justification,
        };
        with_policy_builder(|builder| builder.add_network_rule(rule))?;
        Ok(NoneType)
    }

    fn host_executable<'v>(
        name: &'v str,
        paths: UnpackList<Value<'v>>,
    ) -> anyhow::Result<NoneType> {
        validate_host_executable_name(name)?;

        let mut parsed_paths = Vec::new();
        for value in paths.items {
            let raw = value.unpack_str().ok_or_else(|| {
                Error::InvalidRule(format!(
                    "host_executable paths must be strings (got {})",
                    value.get_type()
                ))
            })?;
            let path = parse_literal_absolute_path(raw)?;
            let Some(path_name) = executable_path_lookup_key(path.as_path()) else {
                return Err(Error::InvalidRule(format!(
                    "host_executable path `{raw}` must have basename `{name}`"
                ))
                .into());
            };
            if path_name != executable_lookup_key(name) {
                return Err(Error::InvalidRule(format!(
                    "host_executable path `{raw}` must have basename `{name}`"
                ))
                .into());
            }
            if !parsed_paths.iter().any(|existing| existing == &path) {
                parsed_paths.push(path);
            }
        }

        with_policy_builder(|builder| {
            builder.add_host_executable(executable_lookup_key(name), parsed_paths);
        })?;
        Ok(NoneType)
    }
}
