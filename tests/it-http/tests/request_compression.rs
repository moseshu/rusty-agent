//! Zstd request-body compression produces decodable wire bytes and the matching headers.
use http::Method;
use http::header::{CONTENT_ENCODING, CONTENT_TYPE, HeaderValue};
use ra_http::{Request, RequestBody, RequestCompression};
use serde_json::json;

fn json_request() -> Request {
    Request::new(Method::POST, "https://example.invalid/v1".to_string())
        .with_json(&json!({ "input": "compress me ".repeat(64), "n": 7 }))
        .with_compression(RequestCompression::Zstd)
}

#[test]
fn zstd_json_body_round_trips_with_encoding_headers() {
    let request = json_request();
    let expected = serde_json::to_vec(request.body.as_ref().unwrap().json().unwrap()).unwrap();

    let prepared = request.prepare_body_for_send().unwrap();

    assert_eq!(prepared.headers.get(CONTENT_ENCODING).unwrap(), "zstd");
    assert_eq!(
        prepared.headers.get(CONTENT_TYPE).unwrap(),
        "application/json"
    );
    let wire = prepared.body_bytes();
    assert!(wire.len() < expected.len());
    assert_eq!(zstd::stream::decode_all(wire.as_ref()).unwrap(), expected);
}

#[test]
fn prepared_request_keeps_compressed_bytes_without_recompressing() {
    let request = json_request();
    let first = request.prepare_body_for_send().unwrap().body_bytes();

    let prepared = request.into_prepared().unwrap();

    assert_eq!(prepared.compression, RequestCompression::None);
    assert_eq!(prepared.headers.get(CONTENT_ENCODING).unwrap(), "zstd");
    let again = prepared.prepare_body_for_send().unwrap();
    assert_eq!(again.body_bytes(), first);
    assert!(matches!(prepared.body, Some(RequestBody::EncodedJson(_))));
}

#[test]
fn compression_rejects_raw_bodies_and_existing_content_encoding() {
    let raw = Request::new(Method::POST, "https://example.invalid/v1".to_string())
        .with_raw_body("raw")
        .with_compression(RequestCompression::Zstd);
    assert_eq!(
        raw.prepare_body_for_send().unwrap_err(),
        "request compression cannot be used with raw bodies"
    );

    let mut encoded = json_request();
    encoded
        .headers
        .insert(CONTENT_ENCODING, HeaderValue::from_static("gzip"));
    assert_eq!(
        encoded.prepare_body_for_send().unwrap_err(),
        "request compression was requested but content-encoding is already set"
    );
}
