use ::http::header::{CONTENT_LENGTH, CONTENT_TYPE, RETRY_AFTER};
use serde_json::Value;

use super::*;
use crate::http::ext_proc;
use crate::proxy::{ProxyResponse, ProxyResponseReason};

/// `ErrorType` of contracts/public/error.schema.json.
const TYPES: [&str; 8] = [
	"invalid_request_error",
	"authentication_error",
	"permission_error",
	"rate_limit_error",
	"quota",
	"capacity",
	"server_error",
	"timeout",
];

/// `ErrorCode` of contracts/public/error.schema.json.
const CODES: [&str; 29] = [
	"bad_request",
	"not_found",
	"payload_too_large",
	"unprocessable",
	"invalid_api_key",
	"expired_api_key",
	"revoked_api_key",
	"insufficient_credit",
	"account_frozen",
	"model_not_entitled",
	"not_in_cohort",
	"user_rate_limited",
	"request_cancelled",
	"quota_exceeded",
	"capacity_exceeded",
	"idempotency_key_reused",
	"unsupported_endpoint",
	"proxy_retired",
	"gateway_unavailable",
	"control_plane_overloaded",
	"credit_gate_unavailable",
	"upstream_contract_violation",
	"unattributed_key",
	"internal",
	"bad_gateway",
	"draining",
	"spool_unhealthy",
	"cold",
	"timeout",
];

/// `AnthropicErrorType` of contracts/public/anthropic-error.schema.json.
const MESSAGES_TYPES: [&str; 10] = [
	"invalid_request_error",
	"authentication_error",
	"permission_error",
	"not_found_error",
	"conflict_error",
	"request_too_large",
	"rate_limit_error",
	"api_error",
	"timeout_error",
	"overloaded_error",
];

async fn json_of(response: Response) -> Value {
	let (_, body) = response.into_parts();
	let bytes = crate::http::read_body_with_limit(body, 1 << 20)
		.await
		.expect("the body reads");
	serde_json::from_slice(&bytes).expect("the body is JSON")
}

/// A body is the OpenAI error object of the public schema: exactly `error{message,type,code,param}`, a
/// message of 1 to 2048 characters, a type and a code of the closed lists, and a null param.
fn assert_openai_envelope(body: &Value) -> (String, Option<String>) {
	let object = body.as_object().expect("the body is an object");
	assert_eq!(object.len(), 1, "only `error`: {body}");
	let error = object["error"].as_object().expect("`error` is an object");
	let mut keys: Vec<&str> = error.keys().map(String::as_str).collect();
	keys.sort_unstable();
	assert_eq!(keys, ["code", "message", "param", "type"], "{body}");
	let message = error["message"].as_str().expect("message is text");
	assert!((1..=2048).contains(&message.chars().count()));
	let kind = error["type"].as_str().expect("type is text");
	assert!(TYPES.contains(&kind), "undeclared type {kind}");
	assert!(error["param"].is_null());
	let code = match &error["code"] {
		Value::Null => None,
		Value::String(code) => {
			assert!(CODES.contains(&code.as_str()), "undeclared code {code}");
			Some(code.clone())
		},
		other => panic!("code is {other}"),
	};
	(kind.to_owned(), code)
}

fn header<'a>(response: &'a Response, name: &str) -> Option<&'a str> {
	response.headers().get(name).and_then(|v| v.to_str().ok())
}

#[test]
fn every_code_has_a_sentence_a_type_and_a_name_of_the_public_vocabulary() {
	let mut seen = std::collections::HashSet::new();
	for code in Code::ALL {
		assert!(
			CODES.contains(&code.wire()),
			"{} is not a public code",
			code.wire()
		);
		assert!(seen.insert(code.wire()), "{} twice", code.wire());
		assert!((1..=2048).contains(&code.message().chars().count()));
		assert!(TYPES.contains(&code.kind().openai()));
		assert!(MESSAGES_TYPES.contains(&code.messages_type()));
	}
	assert_eq!(seen.len(), Code::ALL.len());
}

#[tokio::test]
async fn an_openai_body_is_the_public_envelope_with_a_null_param() {
	let response = PublicError::cold().into_direct_response();
	assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
	assert_eq!(header(&response, "content-type"), Some("application/json"));
	let body = json_of(response).await;
	let (kind, code) = assert_openai_envelope(&body);
	assert_eq!(kind, "server_error");
	assert_eq!(code.as_deref(), Some("cold"));
	assert_eq!(
		body["error"]["message"],
		"The model is starting and is not ready."
	);
}

#[tokio::test]
async fn a_forbidden_answer_has_a_null_code_and_the_permission_type() {
	let mut response = ProxyError::ExternalAuthorizationFailed(None).into_response_with_grpc(false);
	assert_eq!(response.status(), StatusCode::FORBIDDEN);
	assert_eq!(header(&response, "content-type"), Some("application/json"));
	let marker = response.extensions_mut().remove::<PublicError>();
	assert_eq!(marker.map(|m| m.code()), Some(None));
	let (kind, code) = assert_openai_envelope(&json_of(response).await);
	assert_eq!(kind, "permission_error");
	assert_eq!(code, None);
}

/// Errors whose own text names a pod, a host or an internal failure.
const SECRET: &str = "ra-engine-0.pool-small.ra-engines.svc.cluster.local:8000";

fn errors_with_internal_text() -> Vec<ProxyError> {
	vec![
		ProxyError::ProcessingString(SECRET.to_string()),
		ProxyError::Processing(anyhow::anyhow!("{SECRET} refused the connection")),
		ProxyError::ExtProc(ext_proc::Error::BodyMutation(SECRET.to_string())),
		ProxyError::ExtProc(ext_proc::Error::NoMoreResponses),
		ProxyError::SubstrateIngressFailed(StatusCode::BAD_GATEWAY, SECRET.to_string()),
		ProxyError::UpstreamCallTimeout,
		ProxyError::RequestTimeout,
		ProxyError::RouteNotFound,
		ProxyError::NoValidBackends,
		ProxyError::InvalidRequest,
		ProxyError::MethodNotAllowed,
		ProxyError::AuthorizationFailed,
		ProxyError::NoHealthyEndpoints,
		ProxyError::ExternalAuthorizationUnavailable,
		ProxyError::ExternalAuthorizationFailed(Some(StatusCode::PAYLOAD_TOO_LARGE)),
		ProxyError::RequestLimitExceeded,
		ProxyError::RateLimitFailed,
	]
}

#[tokio::test]
async fn a_proxy_error_is_answered_in_fixed_words_and_leaks_nothing() {
	for error in errors_with_internal_text() {
		let display = error.to_string();
		let response = error.into_response_with_grpc(false);
		let status = response.status();
		assert!(
			status.is_client_error() || status.is_server_error(),
			"{display}"
		);
		assert_eq!(
			header(&response, "content-type"),
			Some("application/json"),
			"{display}"
		);
		let body = json_of(response).await;
		assert_openai_envelope(&body);
		let raw = body.to_string();
		for internal in [
			"ra-engine",
			"cluster.local",
			"ext_proc",
			"processing failed",
			"svc",
		] {
			assert!(!raw.contains(internal), "{display}: {raw}");
		}
	}
}

#[tokio::test]
async fn no_healthy_endpoints_is_a_cold_503_with_a_retry_after_of_thirty() {
	let response = ProxyError::NoHealthyEndpoints.into_response_with_grpc(false);
	assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
	assert_eq!(header(&response, "retry-after"), Some("30"));
	let (kind, code) = assert_openai_envelope(&json_of(response).await);
	assert_eq!(
		(kind.as_str(), code.as_deref()),
		("server_error", Some("cold"))
	);
}

#[tokio::test]
async fn an_authorization_call_with_no_answer_is_a_503_credit_gate_unavailable() {
	let response = ProxyError::ExternalAuthorizationUnavailable.into_response_with_grpc(false);
	assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
	assert_eq!(header(&response, "retry-after"), Some("10"));
	let (kind, code) = assert_openai_envelope(&json_of(response).await);
	assert_eq!(
		(kind.as_str(), code.as_deref()),
		("server_error", Some("credit_gate_unavailable"))
	);
	// It is still counted as the external authorization failure it is.
	assert_eq!(
		ProxyResponse::Error(ProxyError::ExternalAuthorizationUnavailable).as_reason(),
		ProxyResponseReason::ExtAuth
	);
}

#[tokio::test]
async fn a_lost_external_processor_is_the_500_internal_envelope() {
	let response =
		ProxyError::ExtProc(ext_proc::Error::NoMoreResponses).into_response_with_grpc(false);
	assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
	assert!(response.headers().get(RETRY_AFTER).is_none());
	let (kind, code) = assert_openai_envelope(&json_of(response).await);
	assert_eq!(
		(kind.as_str(), code.as_deref()),
		("server_error", Some("internal"))
	);
}

#[tokio::test]
async fn a_local_rate_limit_is_a_429_that_says_when_it_resets() {
	let response = ProxyError::RateLimitExceeded {
		limit: 10,
		remaining: 0,
		reset_seconds: 7,
	}
	.into_response_with_grpc(false);
	assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
	assert_eq!(header(&response, "retry-after"), Some("7"));
	let (kind, code) = assert_openai_envelope(&json_of(response).await);
	assert_eq!(
		(kind.as_str(), code.as_deref()),
		("rate_limit_error", Some("user_rate_limited"))
	);
}

#[tokio::test]
async fn a_request_shed_at_the_in_flight_limit_is_a_503_that_says_to_retry() {
	let response = ProxyError::RequestLimitExceeded.into_response_with_grpc(false);
	assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
	assert_eq!(header(&response, "retry-after"), Some("5"));
	let (_, code) = assert_openai_envelope(&json_of(response).await);
	assert_eq!(code.as_deref(), Some("gateway_unavailable"));
}

#[test]
fn a_grpc_client_is_told_the_public_sentence_and_not_the_errors_text() {
	let response =
		ProxyError::ProcessingString("ra-engine-0 is down".to_string()).into_response_with_grpc(true);
	assert_eq!(response.status(), StatusCode::OK);
	assert_eq!(
		header(&response, "grpc-message"),
		Some("The%20gateway%20is%20unavailable.")
	);
	assert!(response.extensions().get::<PublicError>().is_none());
}

#[test]
fn every_error_status_has_a_valid_public_answer() {
	for code in 400u16..=599 {
		let status = StatusCode::from_u16(code).unwrap();
		let public = PublicError::of_status(status);
		assert_eq!(public.status(), status);
		if let Some(code) = public.code() {
			assert!(Code::ALL.contains(&code));
		}
		let body: Value = serde_json::from_slice(&public.render(Dialect::OpenAi, "r")).unwrap();
		assert_openai_envelope(&body);
		if code == 503 {
			assert!(public.retry_after().is_some());
		}
	}
}

#[test]
fn the_messages_body_is_the_messages_envelope_with_the_request_id() {
	let body: Value =
		serde_json::from_slice(&PublicError::cold().render(Dialect::Messages, "req-1")).unwrap();
	let object = body.as_object().unwrap();
	let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
	keys.sort_unstable();
	assert_eq!(keys, ["error", "request_id", "type"]);
	assert_eq!(body["type"], "error");
	assert_eq!(body["request_id"], "req-1");
	assert_eq!(body["error"]["type"], "overloaded_error");
	assert_eq!(
		body["error"]["message"],
		"The model is starting and is not ready."
	);
	assert_eq!(body["error"].as_object().unwrap().len(), 2);
	for code in Code::ALL {
		assert!(MESSAGES_TYPES.contains(&code.messages_type()));
	}
	// A 403 has no code and is a permission error in the Messages form too.
	let forbidden: Value = serde_json::from_slice(
		&PublicError::of_status(StatusCode::FORBIDDEN).render(Dialect::Messages, "r"),
	)
	.unwrap();
	assert_eq!(forbidden["error"]["type"], "permission_error");
}

#[test]
fn the_dialect_of_a_path_is_the_rule_of_the_edges_prerouting_policy() {
	for path in [
		"/v1/messages",
		"/v1/messages/count_tokens",
		"/pools/small/v1/messages",
		"/pools/small/v1/messages/count_tokens",
	] {
		assert_eq!(Dialect::of_path(path), Dialect::Messages, "{path}");
	}
	for path in [
		"/v1/chat/completions",
		"/v1/responses",
		"/v1/decisions",
		"/v1/systemone",
		"/v1/models",
		"/v1/messages/",
		"/v1/messages/other",
		"/other/v1/messages",
		"/pools/small/v1/chat/completions",
		"/",
		"",
	] {
		assert_eq!(Dialect::of_path(path), Dialect::OpenAi, "{path}");
	}
}

#[tokio::test]
async fn finish_writes_a_messages_path_again_with_the_request_id_of_the_response() {
	let mut response = ProxyError::NoHealthyEndpoints.into_response_with_grpc(false);
	response
		.headers_mut()
		.insert("x-request-id", HeaderValue::from_static("req-9"));
	finish(&mut response, Dialect::Messages);
	assert!(response.extensions().get::<PublicError>().is_none());
	assert_eq!(header(&response, "retry-after"), Some("30"));
	assert_eq!(header(&response, "content-type"), Some("application/json"));
	assert!(response.headers().get(CONTENT_LENGTH).is_none());
	let body = json_of(response).await;
	assert_eq!(body["type"], "error");
	assert_eq!(body["request_id"], "req-9");
	assert_eq!(body["error"]["type"], "overloaded_error");
}

#[tokio::test]
async fn finish_quotes_unknown_when_the_response_has_no_usable_request_id() {
	for id in [None, Some(""), Some(&*"x".repeat(257))] {
		let mut response = ProxyError::InvalidRequest.into_response_with_grpc(false);
		if let Some(id) = id {
			response
				.headers_mut()
				.insert("x-request-id", HeaderValue::try_from(id).unwrap());
		}
		finish(&mut response, Dialect::Messages);
		assert_eq!(json_of(response).await["request_id"], UNKNOWN_REQUEST_ID);
	}
}

#[tokio::test]
async fn finish_leaves_the_openai_body_and_an_answer_with_no_marker_alone() {
	let mut openai = ProxyError::NoHealthyEndpoints.into_response_with_grpc(false);
	finish(&mut openai, Dialect::OpenAi);
	assert!(openai.extensions().get::<PublicError>().is_none());
	assert_openai_envelope(&json_of(openai).await);

	// An upstream's own answer, or admission's JSON refusal, carries no marker and stays as it is.
	let mut upstream = ::http::Response::builder()
		.status(StatusCode::SERVICE_UNAVAILABLE)
		.header(CONTENT_TYPE, "application/json")
		.body(Body::from(r#"{"error":{"message":"admission says so"}}"#))
		.unwrap();
	finish(&mut upstream, Dialect::Messages);
	assert_eq!(
		json_of(upstream).await,
		serde_json::json!({"error": {"message": "admission says so"}})
	);
}

fn picker_refusal(status: u16, body: &str, headers: &[(&str, &str)]) -> Response {
	let mut builder = ::http::Response::builder().status(status);
	for (name, value) in headers {
		builder = builder.header(*name, *value);
	}
	builder.body(Body::from(body.to_string())).unwrap()
}

const PICKER_SENTENCE: &str = "inference error: ServiceUnavailable - request timed out in queue and no endpoints are available: request evicted from queue: request TTL expired: no endpoints available";

#[tokio::test]
async fn a_pickers_no_endpoint_refusal_is_wrapped_as_a_cold_503_with_a_retry_after_of_thirty() {
	let mut response = picker_refusal(
		503,
		PICKER_SENTENCE,
		&[
			("x-llm-d-request-dropped-reason", "rejected-no-endpoints"),
			("x-llm-d-flow-queue-duration-ms", "10012"),
			("x-gateway-destination-endpoint-served", "10.4.2.7:8000"),
			("content-length", "168"),
			("x-keep", "yes"),
		],
	);
	seal_processor_refusal(&mut response, PICKER_SENTENCE);
	assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
	assert_eq!(header(&response, "retry-after"), Some("30"));
	assert_eq!(header(&response, "content-type"), Some("application/json"));
	assert_eq!(header(&response, "x-keep"), Some("yes"));
	assert!(response.headers().get(CONTENT_LENGTH).is_none());
	for name in [
		"x-llm-d-request-dropped-reason",
		"x-llm-d-flow-queue-duration-ms",
		"x-gateway-destination-endpoint-served",
	] {
		assert!(response.headers().get(name).is_none(), "{name} was kept");
	}
	let body = json_of(response).await;
	let (kind, code) = assert_openai_envelope(&body);
	assert_eq!(
		(kind.as_str(), code.as_deref()),
		("server_error", Some("cold"))
	);
	let raw = body.to_string();
	assert!(!raw.contains("inference error"));
	assert!(!raw.contains("endpoints"));
}

#[tokio::test]
async fn a_pickers_full_queue_refusal_is_wrapped_as_capacity_exceeded_with_a_retry_after_of_twelve()
{
	let sentence = "inference error: ResourceExhausted - system saturated, sheddable request dropped";
	let mut response = picker_refusal(
		429,
		sentence,
		&[("x-llm-d-request-dropped-reason", "rejected-saturated")],
	);
	seal_processor_refusal(&mut response, sentence);
	assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
	assert_eq!(header(&response, "retry-after"), Some("12"));
	let (kind, code) = assert_openai_envelope(&json_of(response).await);
	assert_eq!(
		(kind.as_str(), code.as_deref()),
		("capacity", Some("capacity_exceeded"))
	);
}

#[tokio::test]
async fn a_picker_shutting_down_is_wrapped_as_draining_and_its_own_retry_after_is_kept() {
	let mut response = picker_refusal(
		503,
		"inference error: ServiceUnavailable - shutting down",
		&[
			("x-llm-d-request-dropped-reason", "rejected-shutting-down"),
			("retry-after", "3"),
		],
	);
	seal_processor_refusal(
		&mut response,
		"inference error: ServiceUnavailable - shutting down",
	);
	assert_eq!(header(&response, "retry-after"), Some("3"));
	let (_, code) = assert_openai_envelope(&json_of(response).await);
	assert_eq!(code.as_deref(), Some("draining"));
}

#[tokio::test]
async fn another_status_of_a_plain_text_processor_answer_gets_the_sentence_of_its_status() {
	for (status, code) in [
		(400u16, "bad_request"),
		(404, "not_found"),
		(500, "internal"),
		(502, "bad_gateway"),
	] {
		let mut response = picker_refusal(status, "inference error: whatever", &[]);
		seal_processor_refusal(&mut response, "inference error: whatever");
		assert_eq!(response.status().as_u16(), status);
		let (_, got) = assert_openai_envelope(&json_of(response).await);
		assert_eq!(got.as_deref(), Some(code));
	}
}

#[tokio::test]
async fn a_json_answer_of_a_processor_is_left_as_it_is() {
	let envelope = r#"{"error":{"message":"The credit check could not be completed.","type":"server_error","code":"credit_gate_unavailable","param":null}}"#;
	// With its content type.
	let mut with_type = picker_refusal(
		503,
		envelope,
		&[("content-type", "application/json"), ("retry-after", "10")],
	);
	seal_processor_refusal(&mut with_type, envelope);
	assert!(with_type.extensions().get::<PublicError>().is_none());
	assert_eq!(header(&with_type, "retry-after"), Some("10"));
	assert_eq!(
		json_of(with_type).await["error"]["code"],
		"credit_gate_unavailable"
	);
	// Without a content type, when the body is a JSON object.
	let mut without_type = picker_refusal(503, envelope, &[]);
	seal_processor_refusal(&mut without_type, envelope);
	assert!(without_type.extensions().get::<PublicError>().is_none());
	assert!(without_type.headers().get(RETRY_AFTER).is_none());
	assert_eq!(
		json_of(without_type).await["error"]["code"],
		"credit_gate_unavailable"
	);
	// A JSON text that is not an object is not an envelope.
	let mut array = picker_refusal(500, "[1,2]", &[]);
	seal_processor_refusal(&mut array, "[1,2]");
	assert_openai_envelope(&json_of(array).await);
}

#[tokio::test]
async fn an_answer_that_is_not_an_error_status_is_never_rewritten() {
	for status in [200u16, 204, 302] {
		let mut response = picker_refusal(status, "plain text", &[]);
		seal_processor_refusal(&mut response, "plain text");
		assert!(response.extensions().get::<PublicError>().is_none());
		assert!(response.headers().get(CONTENT_TYPE).is_none());
		let (_, body) = response.into_parts();
		let bytes = crate::http::read_body_with_limit(body, 1024).await.unwrap();
		assert_eq!(bytes.as_ref(), b"plain text");
	}
}

#[tokio::test]
async fn a_failed_authorization_service_with_a_configured_status_gets_the_sentence_of_that_status()
{
	let response = PublicError::of_status(StatusCode::SERVICE_UNAVAILABLE).into_direct_response();
	assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
	assert_eq!(header(&response, "retry-after"), Some("10"));
	let raw = json_of(response).await.to_string();
	assert!(!raw.contains("Authorization service"));
}
