//! The edge's own errors, as the public error envelope.
//!
//! Every error answer the proxy makes itself used to be the error's Display text as `text/plain`:
//! `upstream call failed: ...` with the upstream's address, `ext_proc failed: ...`, `external authorization
//! failed`, `no healthy backends`. An endpoint picker's refusal, which the proxy hands on as a direct
//! response, was the picker's own sentence, again with no content type. This module is the one place that
//! turns such an answer into the body a client of the platform reads: the OpenAI error object
//! `{"error":{"message","type","code","param"}}`, or the Messages error object
//! `{"type":"error","error":{"type","message"},"request_id"}` on the Messages paths. Both are the platform's
//! public contract (`contracts/public/error.schema.json`, `anthropic-error.schema.json`,
//! `dialect-errors.json` of InferenceInfra).
//!
//! The rules:
//! - The words are fixed. A message is one sentence of a closed table, chosen by the error and its status. It
//!   never carries the error's own text, a host, an address, a pod or pool name, or anything the peer said.
//! - The status is the status the proxy already gave. The one status that changes is the answer to an
//!   external authorization call that got no answer ([`ProxyError::ExternalAuthorizationUnavailable`]): it
//!   was a 403, which says the caller is not entitled, and is now the 503 `credit_gate_unavailable` that
//!   admission itself sends when its own authorize fails.
//! - A 503 says when to come back (`Retry-After`), as the platform's other refusals do.
//! - An answer that already is JSON (admission's and the meter's own refusals) is left alone, byte for byte.
//!   A processor's plain-text error is wrapped; its internal headers (`x-llm-d-*`, `x-gateway-*`) are dropped.
//!
//! The body is first written in the OpenAI form, where the error is made. The request's dialect is known only
//! by the path, and its request id only after the gateway's response policies have run, so [`finish`] runs at
//! the end of the request and, for a Messages path, writes the body again in the Messages form.

use ::http::header::{
	CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, RETRY_AFTER, TRANSFER_ENCODING,
};
use bytes::Bytes;

use crate::http::{Body, HeaderMap, HeaderName, HeaderValue, Response, StatusCode};
use crate::proxy::ProxyError;

/// What a Messages error body carries as its `request_id` when the request has none to quote yet.
pub const UNKNOWN_REQUEST_ID: &str = "unknown";
/// The response header the gateway's own response policy sets to the request's id.
const REQUEST_ID_HEADER: &str = "x-request-id";
/// The longest `request_id` the Messages schema accepts.
const MAX_REQUEST_ID_BYTES: usize = 256;
/// The header an endpoint picker sets to say why it dropped a request.
const PICKER_DROPPED_REASON: &str = "x-llm-d-request-dropped-reason";
/// The picker's reason for a request refused while it is shutting down.
const PICKER_SHUTTING_DOWN: &str = "rejected-shutting-down";
/// Header name prefixes of the endpoint picker and the gateway API inference extension. They are internal and
/// are dropped from an error the proxy wraps.
const INTERNAL_HEADER_PREFIXES: [&str; 2] = ["x-llm-d-", "x-gateway-"];

/// Seconds to wait after a 503 the proxy cannot say more about.
const RETRY_UNAVAILABLE: u32 = 10;
/// Seconds to wait after the pool has no ready engine (a cold start is long).
const RETRY_COLD: u32 = 30;
/// Seconds to wait after the pool is full (the engine's own capacity hint).
const RETRY_CAPACITY: u32 = 12;
/// Seconds to wait after the proxy shed a request at its in-flight limit.
const RETRY_BUSY: u32 = 5;

/// The shape of an error body. A client names its dialect by the path it calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialect {
	/// `{"error":{"message","type","code","param"}}`: Chat Completions, Responses, Decisions.
	OpenAi,
	/// `{"type":"error","error":{"type","message"},"request_id"}`: Messages.
	Messages,
}

impl Dialect {
	/// The dialect of a request path. The edge's pre-routing policy decides the `x-ra-dialect` header from the
	/// path alone with the same rule (`ra-public-prerouting` of InferenceInfra), and this must stay equal to it:
	/// the Messages pair, bare or under the qualification route's `/pools/<pool>` prefix.
	pub fn of_path(path: &str) -> Self {
		let messages = path == "/v1/messages"
			|| path == "/v1/messages/count_tokens"
			|| (path.starts_with("/pools/")
				&& (path.ends_with("/v1/messages") || path.ends_with("/v1/messages/count_tokens")));
		if messages {
			Dialect::Messages
		} else {
			Dialect::OpenAi
		}
	}
}

/// The `error.type` of the OpenAI form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
	InvalidRequest,
	Authentication,
	Permission,
	RateLimit,
	Capacity,
	Server,
	Timeout,
}

impl Kind {
	fn openai(self) -> &'static str {
		match self {
			Kind::InvalidRequest => "invalid_request_error",
			Kind::Authentication => "authentication_error",
			Kind::Permission => "permission_error",
			Kind::RateLimit => "rate_limit_error",
			Kind::Capacity => "capacity",
			Kind::Server => "server_error",
			Kind::Timeout => "timeout",
		}
	}
}

/// The public error codes the edge can raise: a subset of the closed `ErrorCode` enum of
/// `contracts/public/error.schema.json`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Code {
	BadRequest,
	NotFound,
	PayloadTooLarge,
	Unprocessable,
	InvalidApiKey,
	UserRateLimited,
	CapacityExceeded,
	UnsupportedEndpoint,
	GatewayUnavailable,
	CreditGateUnavailable,
	Internal,
	BadGateway,
	Draining,
	Cold,
	Timeout,
}

impl Code {
	/// Every code, so a test can walk the table.
	pub const ALL: [Code; 15] = [
		Code::BadRequest,
		Code::NotFound,
		Code::PayloadTooLarge,
		Code::Unprocessable,
		Code::InvalidApiKey,
		Code::UserRateLimited,
		Code::CapacityExceeded,
		Code::UnsupportedEndpoint,
		Code::GatewayUnavailable,
		Code::CreditGateUnavailable,
		Code::Internal,
		Code::BadGateway,
		Code::Draining,
		Code::Cold,
		Code::Timeout,
	];

	/// The value of `error.code`.
	pub fn wire(self) -> &'static str {
		match self {
			Code::BadRequest => "bad_request",
			Code::NotFound => "not_found",
			Code::PayloadTooLarge => "payload_too_large",
			Code::Unprocessable => "unprocessable",
			Code::InvalidApiKey => "invalid_api_key",
			Code::UserRateLimited => "user_rate_limited",
			Code::CapacityExceeded => "capacity_exceeded",
			Code::UnsupportedEndpoint => "unsupported_endpoint",
			Code::GatewayUnavailable => "gateway_unavailable",
			Code::CreditGateUnavailable => "credit_gate_unavailable",
			Code::Internal => "internal",
			Code::BadGateway => "bad_gateway",
			Code::Draining => "draining",
			Code::Cold => "cold",
			Code::Timeout => "timeout",
		}
	}

	fn kind(self) -> Kind {
		match self {
			Code::BadRequest
			| Code::NotFound
			| Code::PayloadTooLarge
			| Code::Unprocessable
			| Code::UnsupportedEndpoint => Kind::InvalidRequest,
			Code::InvalidApiKey => Kind::Authentication,
			Code::UserRateLimited => Kind::RateLimit,
			Code::CapacityExceeded => Kind::Capacity,
			Code::GatewayUnavailable
			| Code::CreditGateUnavailable
			| Code::Internal
			| Code::BadGateway
			| Code::Draining
			| Code::Cold => Kind::Server,
			Code::Timeout => Kind::Timeout,
		}
	}

	/// The sentence of the code: the one `dialect-errors.json` gives the same code in the Messages dialect,
	/// so a client reads the same words from every hop.
	pub fn message(self) -> &'static str {
		match self {
			Code::BadRequest => "The request is invalid.",
			Code::NotFound => "The requested resource could not be found.",
			Code::PayloadTooLarge => "The request body is over the size limit.",
			Code::Unprocessable => "The request could not be processed.",
			Code::InvalidApiKey => "The API key is missing, malformed or unknown.",
			Code::UserRateLimited => "The key has reached its rate limit.",
			Code::CapacityExceeded => "No capacity is free for this model right now.",
			Code::UnsupportedEndpoint => "This model does not support this endpoint.",
			Code::GatewayUnavailable => "The gateway is unavailable.",
			Code::CreditGateUnavailable => "The credit check could not be completed.",
			Code::Internal => "An internal error occurred.",
			Code::BadGateway => "The model engine failed.",
			Code::Draining => "The model is draining and accepts no new requests.",
			Code::Cold => "The model is starting and is not ready.",
			Code::Timeout => "The request timed out.",
		}
	}

	/// The `error.type` of the Messages form.
	fn messages_type(self) -> &'static str {
		match self {
			Code::NotFound => "not_found_error",
			Code::PayloadTooLarge => "request_too_large",
			Code::CapacityExceeded | Code::Draining | Code::Cold => "overloaded_error",
			Code::Timeout => "timeout_error",
			Code::GatewayUnavailable
			| Code::CreditGateUnavailable
			| Code::Internal
			| Code::BadGateway => "api_error",
			Code::BadRequest | Code::Unprocessable | Code::UnsupportedEndpoint => "invalid_request_error",
			Code::InvalidApiKey => "authentication_error",
			Code::UserRateLimited => "rate_limit_error",
		}
	}
}

/// The sentence of a 403 that has no code of its own in the public vocabulary.
const FORBIDDEN_MESSAGE: &str = "The request is not permitted.";

/// One error answer, decided: its status, its public code (a 403 has none; `code` is then null), and when to
/// come back. It is also the marker the proxy leaves on the response so that [`finish`] can write the body
/// again for the request's dialect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PublicError {
	status: StatusCode,
	code: Option<Code>,
	retry_after: Option<u32>,
}

impl PublicError {
	/// The answer for a status, in the public vocabulary. A 503 comes with a `Retry-After`.
	pub fn of_status(status: StatusCode) -> Self {
		let (code, retry_after) = match status.as_u16() {
			401 | 407 => (Some(Code::InvalidApiKey), None),
			403 => (None, None),
			404 => (Some(Code::NotFound), None),
			405 => (Some(Code::UnsupportedEndpoint), None),
			408 | 504 => (Some(Code::Timeout), None),
			413 => (Some(Code::PayloadTooLarge), None),
			422 => (Some(Code::Unprocessable), None),
			429 => (Some(Code::UserRateLimited), None),
			500 => (Some(Code::Internal), None),
			502 => (Some(Code::BadGateway), None),
			503 => (Some(Code::GatewayUnavailable), Some(RETRY_UNAVAILABLE)),
			400..=499 => (Some(Code::BadRequest), None),
			_ => (Some(Code::Internal), None),
		};
		PublicError {
			status,
			code,
			retry_after,
		}
	}

	/// The pool has no engine that can take the request (503 `cold`).
	pub fn cold() -> Self {
		Self::with(
			StatusCode::SERVICE_UNAVAILABLE,
			Code::Cold,
			Some(RETRY_COLD),
		)
	}

	/// The pool is full (429 `capacity_exceeded`).
	pub fn capacity() -> Self {
		Self::with(
			StatusCode::TOO_MANY_REQUESTS,
			Code::CapacityExceeded,
			Some(RETRY_CAPACITY),
		)
	}

	/// The model is draining (503 `draining`).
	pub fn draining() -> Self {
		Self::with(
			StatusCode::SERVICE_UNAVAILABLE,
			Code::Draining,
			Some(RETRY_COLD),
		)
	}

	/// The external authorization service gave no answer (503 `credit_gate_unavailable`, as admission's own).
	pub fn credit_gate_unavailable() -> Self {
		Self::with(
			StatusCode::SERVICE_UNAVAILABLE,
			Code::CreditGateUnavailable,
			Some(RETRY_UNAVAILABLE),
		)
	}

	/// The proxy shed the request at its in-flight limit (503 `gateway_unavailable`).
	pub fn busy() -> Self {
		Self::with(
			StatusCode::SERVICE_UNAVAILABLE,
			Code::GatewayUnavailable,
			Some(RETRY_BUSY),
		)
	}

	/// A local rate limit refused the request; it resets in `reset_seconds`.
	pub fn rate_limited(reset_seconds: u64) -> Self {
		let reset = u32::try_from(reset_seconds).unwrap_or(u32::MAX).max(1);
		Self::with(
			StatusCode::TOO_MANY_REQUESTS,
			Code::UserRateLimited,
			Some(reset),
		)
	}

	fn with(status: StatusCode, code: Code, retry_after: Option<u32>) -> Self {
		PublicError {
			status,
			code: Some(code),
			retry_after,
		}
	}

	/// The answer for an error of the proxy, whose status is `status` (decided by the caller from the same
	/// error, so the two cannot differ).
	pub fn of_proxy_error(error: &ProxyError, status: StatusCode) -> Self {
		match error {
			ProxyError::NoHealthyEndpoints | ProxyError::DnsResolution => Self::cold(),
			ProxyError::ExternalAuthorizationUnavailable => Self::credit_gate_unavailable(),
			ProxyError::RequestLimitExceeded => Self::busy(),
			ProxyError::RateLimitExceeded { reset_seconds, .. } => Self::rate_limited(*reset_seconds),
			_ => Self::of_status(status),
		}
	}

	pub fn status(&self) -> StatusCode {
		self.status
	}

	pub fn code(&self) -> Option<Code> {
		self.code
	}

	pub fn retry_after(&self) -> Option<u32> {
		self.retry_after
	}

	/// The sentence of the answer.
	pub fn message(&self) -> &'static str {
		self.code.map_or(FORBIDDEN_MESSAGE, Code::message)
	}

	fn kind(&self) -> Kind {
		self.code.map_or(Kind::Permission, Code::kind)
	}

	/// The body in `dialect`. `request_id` is quoted by the Messages form only.
	pub fn render(&self, dialect: Dialect, request_id: &str) -> Bytes {
		let body = match dialect {
			Dialect::OpenAi => serde_json::json!({
				"error": {
					"message": self.message(),
					"type": self.kind().openai(),
					"code": self.code.map(Code::wire),
					"param": serde_json::Value::Null,
				}
			}),
			Dialect::Messages => serde_json::json!({
				"type": "error",
				"error": {
					"type": self.code.map_or("permission_error", Code::messages_type),
					"message": self.message(),
				},
				"request_id": request_id,
			}),
		};
		Bytes::from(body.to_string())
	}

	/// Set the headers of the answer: JSON, and `Retry-After` unless the answer already has one. The headers
	/// that described the old body are dropped.
	fn set_headers(&self, headers: &mut HeaderMap) {
		headers.remove(CONTENT_LENGTH);
		headers.remove(CONTENT_ENCODING);
		headers.remove(TRANSFER_ENCODING);
		headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
		if let Some(seconds) = self.retry_after
			&& !headers.contains_key(RETRY_AFTER)
			&& let Ok(value) = HeaderValue::try_from(seconds.to_string())
		{
			headers.insert(RETRY_AFTER, value);
		}
	}

	/// Finish a response builder that already has its status (and any headers of the error, such as a
	/// challenge or a rate limit's counters) into the answer, and leave the marker for [`finish`].
	pub fn into_response(self, mut builder: ::http::response::Builder) -> Response {
		if let Some(headers) = builder.headers_mut() {
			self.set_headers(headers);
		}
		let mut response = builder
			.body(Body::from(self.render(Dialect::OpenAi, UNKNOWN_REQUEST_ID)))
			.expect("a public error response is valid");
		response.extensions_mut().insert(self);
		response
	}

	/// The answer as a response of its own, for a refusal made where no response was being built.
	pub fn into_direct_response(self) -> Response {
		let builder = ::http::Response::builder().status(self.status);
		self.into_response(builder)
	}
}

/// Write the body of an error the proxy made again in the request's dialect, quoting the request id the
/// gateway's response policy put on the response. It runs once at the end of the request, after the response
/// policies, and takes the marker off the response. An answer that carries no marker (an upstream's, or a JSON
/// refusal of admission or the meter) is not touched. The OpenAI form is already written, so only a Messages
/// path has anything to do.
pub fn finish(response: &mut Response, dialect: Dialect) {
	let Some(public) = response.extensions_mut().remove::<PublicError>() else {
		return;
	};
	if dialect == Dialect::OpenAi {
		return;
	}
	let request_id = response
		.headers()
		.get(REQUEST_ID_HEADER)
		.and_then(|value| value.to_str().ok())
		.filter(|value| !value.is_empty() && value.len() <= MAX_REQUEST_ID_BYTES)
		.unwrap_or(UNKNOWN_REQUEST_ID)
		.to_owned();
	*response.body_mut() = Body::from(public.render(dialect, &request_id));
	response.headers_mut().remove(CONTENT_LENGTH);
}

/// Whether a content type names JSON (`application/json`, or a `+json` type).
fn is_json_content_type(headers: &HeaderMap) -> bool {
	let Some(value) = headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok()) else {
		return false;
	};
	let essence = value
		.split(';')
		.next()
		.unwrap_or_default()
		.trim()
		.to_ascii_lowercase();
	essence == "application/json" || essence.ends_with("+json")
}

/// Whether a text is one JSON object, so that a processor that forgot its content type is not rewritten.
fn is_json_object(text: &str) -> bool {
	serde_json::from_str::<serde_json::Value>(text).is_ok_and(|value| value.is_object())
}

/// A processor's immediate response, made the public envelope when it is an error and not JSON.
///
/// An external processor can answer a request itself. The platform's own (admission, the stream meter) answers
/// JSON, and that is left as it is. The one that does not is the endpoint picker: it answers its own sentence
/// (`inference error: ServiceUnavailable - request timed out in queue and no endpoints are available ...`)
/// with no content type, a 429 when its queue is full and a 503 when the pool has no endpoint. Such an answer
/// is rewritten to the public envelope with the same status, the picker's headers (`x-llm-d-*`, `x-gateway-*`)
/// are dropped, and `Retry-After` is set unless the picker set one. `body` is the text the processor sent.
pub fn seal_processor_refusal(response: &mut Response, body: &str) {
	let status = response.status();
	if !(status.is_client_error() || status.is_server_error()) {
		return;
	}
	if is_json_content_type(response.headers()) || is_json_object(body) {
		return;
	}
	let reason = response
		.headers()
		.get(PICKER_DROPPED_REASON)
		.and_then(|value| value.to_str().ok())
		.map(str::to_owned);
	let public = match (status.as_u16(), reason.as_deref()) {
		(429, _) => PublicError::capacity(),
		(503, Some(PICKER_SHUTTING_DOWN)) => PublicError::draining(),
		(503, _) => PublicError::cold(),
		_ => PublicError::of_status(status),
	};
	drop_internal_headers(response.headers_mut());
	public.set_headers(response.headers_mut());
	*response.body_mut() = Body::from(public.render(Dialect::OpenAi, UNKNOWN_REQUEST_ID));
	response.extensions_mut().insert(public);
}

fn drop_internal_headers(headers: &mut HeaderMap) {
	let internal: Vec<HeaderName> = headers
		.keys()
		.filter(|name| {
			INTERNAL_HEADER_PREFIXES
				.iter()
				.any(|prefix| name.as_str().starts_with(prefix))
		})
		.cloned()
		.collect();
	for name in internal {
		headers.remove(name);
	}
}

#[cfg(test)]
#[path = "public_error_tests.rs"]
mod tests;
