// Adapted from openai/codex (Apache-2.0) @ e58bf8f5233b51e7cd1679d947e3028c1ed91b06, modified.
//! Detects Bearer insufficient-scope challenges without recording response headers.
type AuthParameter<'a> = (&'a str, Option<String>);
type ChallengeStart<'a> = (&'a str, Option<AuthParameter<'a>>);

#[derive(Default)]
enum Parameter {
    #[default]
    Missing,
    Value(String),
    Invalid,
}

#[derive(Default)]
struct BearerChallenge {
    error: Parameter,
}

impl BearerChallenge {
    fn add_parameter(&mut self, name: &str, value: Option<String>) {
        let parameter = if name.eq_ignore_ascii_case("error") {
            &mut self.error
        } else {
            return;
        };

        *parameter = match (&*parameter, value) {
            (Parameter::Missing, Some(value)) => Parameter::Value(value),
            (Parameter::Missing, None) | (Parameter::Value(_) | Parameter::Invalid, _) => {
                Parameter::Invalid
            }
        };
    }

    fn insufficient_scope(&self) -> bool {
        matches!(&self.error, Parameter::Value(error) if error == "insufficient_scope")
    }
}

pub(super) fn has_insufficient_scope(headers: &reqwest::header::HeaderMap) -> bool {
    headers
        .get_all(reqwest::header::WWW_AUTHENTICATE)
        .iter()
        .filter_map(|header| header.to_str().ok())
        .any(|header| parse_bearer_insufficient_scope(header) == Some(true))
}

fn parse_bearer_insufficient_scope(header: &str) -> Option<bool> {
    let segments = split_unquoted_segments(header)?;
    let mut bearer_challenge: Option<BearerChallenge> = None;

    for segment in segments {
        if let Some((name, value)) = parse_auth_param(segment) {
            if let Some(challenge) = bearer_challenge.as_mut() {
                challenge.add_parameter(name, value);
            }
            continue;
        }

        if bearer_challenge
            .take()
            .is_some_and(|challenge| challenge.insufficient_scope())
        {
            return Some(true);
        }

        let (scheme, parameter) = parse_challenge_start(segment)?;
        if scheme.eq_ignore_ascii_case("Bearer") {
            let mut challenge = BearerChallenge::default();
            if let Some((name, value)) = parameter {
                challenge.add_parameter(name, value);
            }
            bearer_challenge = Some(challenge);
        }
    }

    Some(bearer_challenge.is_some_and(|challenge| challenge.insufficient_scope()))
}

fn parse_challenge_start(segment: &str) -> Option<ChallengeStart<'_>> {
    let segment = segment.trim();
    let parameter_start = segment.find(char::is_whitespace);
    let (scheme, parameter) = match parameter_start {
        Some(parameter_start) => (
            &segment[..parameter_start],
            parse_auth_param(&segment[parameter_start..]),
        ),
        None => (segment, None),
    };

    is_http_token(scheme).then_some((scheme, parameter))
}

fn parse_auth_param(segment: &str) -> Option<AuthParameter<'_>> {
    let (name, value) = segment.trim().split_once('=')?;
    let name = name.trim();
    is_http_token(name).then_some((name, parse_auth_param_value(value.trim())))
}

fn parse_auth_param_value(value: &str) -> Option<String> {
    if let Some(quoted_value) = value.strip_prefix('"') {
        let quoted_value = quoted_value.strip_suffix('"')?;
        let mut decoded = String::with_capacity(quoted_value.len());
        let mut characters = quoted_value.chars();
        while let Some(character) = characters.next() {
            if character == '\\' {
                decoded.push(characters.next()?);
            } else {
                decoded.push(character);
            }
        }
        Some(decoded)
    } else {
        is_http_token(value).then(|| value.to_string())
    }
}

fn split_unquoted_segments(header: &str) -> Option<Vec<&str>> {
    let mut segments = Vec::new();
    let mut segment_start = 0;
    let mut in_quotes = false;
    let mut escaped = false;

    for (position, character) in header.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' if in_quotes => escaped = true,
            '"' => in_quotes = !in_quotes,
            ',' | ';' if !in_quotes => {
                segments.push(&header[segment_start..position]);
                segment_start = position + character.len_utf8();
            }
            _ => {}
        }
    }

    if in_quotes || escaped {
        None
    } else {
        segments.push(&header[segment_start..]);
        Some(segments)
    }
}

fn is_http_token(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}
