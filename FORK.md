# RunanywhereAI fork of agentgateway

This branch, `ra/v1.6.0-ra.1`, is upstream tag `v1.6.0`
(commit ea5608642b9d5c94c5baf5fd9f2f9849b808e963) plus the same four changes and their tests as the `v1.5.0` series
(`ra/v1.5.0-ra.3`, rebased onto `v1.6.0`, see "The rebase onto v1.6.0" below). Nothing else differs.
The `v1.5.0` branches stay as they were: `ra/v1.5.0-ra.3` and its picked-destination-wait follow-ups, `ra/v1.5.0-ra.2`
(the first two changes, the build `v1.5.0-ra.2`) and `ra/v1.5.0-client-leave` (the first only, the build `v1.5.0-ra.1`).
The build of this branch reports `v1.6.0-ra.1`.

## What it carries

### 1. Cancel the upstream, and the ext_proc call, when the client leaves

When an ext_proc policy processes the response body in `FULL_DUPLEX_STREAMED` mode and the downstream
client disconnects after the response headers were sent, upstream (v1.5.0 and v1.6.0) keeps reading the upstream body
to its end in a detached task, and the ext_proc stream keeps receiving every chunk. This change ties that
background work to ownership of the processed response body: a per-instance cancellation token, cancelled
when the returned body is dropped (a drop guard held by the processed body: in v1.6.0 a content wrapper installed with
`Body::transform_stream`, where v1.5.0 used the crate's own `http::DropBody`), aborts the upstream-body
reader (dropping the upstream body, which cancels the upstream request), ends the gRPC request stream to
the ext_proc server (`take_until` on the token) and stops the response continuation.

A client that leaves is told apart from a response that was processed to its end: when the returned body is
dropped while the processing of it is still running (a sender of the body's channel is alive), a second token
(`abort`) is cancelled first. It cancels the call to the ext_proc server instead of ending it: the request body of the
call (`AbortableChannel` in `ext_proc/channel.rs`) fails with an `h2::Error` of reason CANCEL
(`agent_hbone::cancelled()`), which hyper turns into RST_STREAM(CANCEL), and the task that reads the stream of
responses stops and drops it. The server sees a cancelled call (for a Go gRPC server, the stream's context is
cancelled and `Recv` returns an error, not `io.EOF`), where a half-close shows it a clean end. A tonic server
reports a cancelled request body as a clean end of the request stream (its decoder maps a `Cancelled` body error to the
end), so the tests tell the two apart by the server's response stream: the connection drops it at once for a cancelled
call and leaves it open for the client to read after a half-close. A response processed to its end still closes the call
cleanly.

Files: `crates/agentgateway/src/http/ext_proc.rs` (the fix), `ext_proc/channel.rs` (the abortable channel),
`crates/hbone/src/lib.rs` (`cancelled()`, which saves a dependency edit), `crates/agentgateway/src/http/ext_proc_tests.rs`
(the tests `dropping_the_response_cancels_the_upstream_and_the_ext_proc_call` and
`a_response_read_to_its_end_closes_the_ext_proc_stream_cleanly`) and
`crates/agentgateway/src/test_helpers/extprocmock.rs` (`on_eof`, which is given the response stream, and
`on_stream_error` hooks on the mock handler).

### 2. A body field read never materialises a large or non-scalar value

`jsonField` is what the expression optimizer makes of `json(x).f`. It streamed to the key and then built the
whole value as a `serde_json::Value` and a CEL value, so a request of 8 MiB whose `model` is an array of tiny
objects cost the proxy about 450 MiB, before any credential was checked. `jsonField` now accepts only a string
of at most 4096 bytes, a number, a boolean or null: an array or an object is skipped with `IgnoredAny`
(nothing is kept) and an over-long string is dropped, and either one is a CEL error, the same kind of error a
missing key is, so `coalesce(json(x).f, -1)` falls back. The last occurrence of a repeated key still decides.
A value that is accepted is converted exactly as before.

Files: `crates/celx/src/general.rs` (the fix), `crates/celx/src/function_tests.rs` (six unit tests whose names
start `json_field_`) and the new `crates/celx/tests/json_field_memory.rs` (a counting global allocator
measures the peak while `jsonField` reads `model` from a crafted 8 MiB body; it must stay under twice the body).

### 3. The proxy's own errors are the platform's public error envelope

Every error answer the proxy made itself was the error's Display text as `text/plain` (`upstream call failed: ...`
with the upstream's address, `ext_proc failed: ...`, `no healthy backends`), and an external processor's refusal
(the endpoint picker's `inference error: ServiceUnavailable - ... no endpoints are available ...`) reached the client
as the processor's own sentence with no content type. A client of the platform reads the OpenAI error object
`{"error":{"message","type","code","param"}}` (or the Messages error object on the Messages paths), and nothing
internal may reach it. The new module `proxy/public_error.rs` is the one place that writes such a body:

- `ProxyError::into_response_with_grpc` answers an HTTP error status with the envelope and a sentence of a closed
  table (the same words `dialect-errors.json` of the consumer gives each code), chosen by the error and its status;
  `grpc-message` carries the same sentence. The status is unchanged, except as below. A 503 carries `Retry-After`
  (10 s; 30 s with no healthy backend, code `cold`; 5 s when the proxy sheds a request at its in-flight limit).
- A new error `ExternalAuthorizationUnavailable`: the external authorization call got no answer and the policy fails
  closed. It was a 403 ("external authorization failed"), which tells the caller it is not entitled. It is now the 503
  `credit_gate_unavailable` with `Retry-After: 10`, the answer admission itself gives when its own authorize fails.
  Its metrics reason stays `ExtAuth`. A response with status UNAVAILABLE and no custom response at all (admission's
  `unavailable()`, a failure it cannot classify) is the same 503, decided before the cache is written so a transient fault is
  never a cached refusal; a denial that carries a response, including admission's own 503 envelopes (also status UNAVAILABLE),
  is passed on untouched, and every other code without a response is still `ExternalAuthorizationFailed` (403, no code).
- `to_immediate_response` (an external processor answering the request itself) wraps an error answer that is not JSON
  (no JSON content type and a body that is not one JSON object) in the envelope: a 429 is `capacity_exceeded`
  (`Retry-After: 12`), a 503 is `cold` (`Retry-After: 30`; `draining` when the picker says it is shutting down), any other
  status gets the sentence of its status. The picker's own `x-llm-d-*` and `x-gateway-*` headers are dropped, and a
  `Retry-After` the processor sent is kept. A JSON answer (admission's and the meter's) is not touched, byte for byte.
- The body is written in the OpenAI form where the error is made. `finish` runs at the end of the request, after the
  response policies, and for a Messages path (`/v1/messages`, `/v1/messages/count_tokens`, bare or under `/pools/<pool>`;
  the rule of the consumer's pre-routing policy) writes the Messages form again, with the `x-request-id` the response
  policy set (`unknown` when there is none).

Not changed: protocol-level answers hyper writes before the proxy sees a request, the MCP and LLM provider error shapes,
`RemoteRateLimitExceeded` and guardrail rejections (a service's own response), the CSRF policy's 403, and the LLM `Models`
route's 501: the consumer's edge uses none of them.

Files: `crates/agentgateway/src/proxy/public_error.rs` (new, with `public_error_tests.rs`), `proxy/mod.rs`,
`proxy/httpproxy.rs`, `proxy/gateway.rs`, `http/ext_proc/mutation.rs`, `http/ext_authz.rs`, `http/substrate/ingress.rs` (the stale-assignment 503), and the tests that asserted the old
text (`http/ext_proc_tests.rs`, `tests/tests/policy.rs`, `auth.rs`, `dfp.rs`).

### 4. A request waits for the pod the endpoint picker chose

The endpoint picker follows the cluster's pods directly and the proxy's endpoint set follows them through the controller, so the
picker can name a pod the proxy has not been told about yet, and the call failed with "no healthy backends" for a pod that was Ready
and serving. `build_service_call_for_picked_destination` (`proxy/httpproxy.rs`) repeats the endpoint lookup until the workload store
inserts the pod, for at most two seconds (`PICKED_DESTINATION_WAIT`), with one last run at the deadline; the picker is not asked
again and a destination still unknown fails exactly as before. The deadline is checked after every lookup, so inserts of other
workloads cannot stretch the wait past the limit (`inserts_that_never_stop_do_not_extend_the_wait_past_the_limit`). Only a destination the picker chose for a Service backend (not a
passthrough one) waits. Tests: `proxy::httpproxy::picked_destination_wait_tests::*` and
`http::ext_proc::tests::standalone_inference_routing::standalone_inference_routing_waits_for_the_selected_destination_to_join_the_service`.
(Commit `fe6e7bad`, by the consumer's picker-readiness work.)

## Why

Behind this gateway an inference engine keeps generating for a client that has gone, and the metering
ext_proc server charges for tokens the client never received.

## Evidence

- `v1.6.0-ra.1` (this branch): the tests are Cloud Build runs of the consumer's ad-hoc test builds (the consumer's evidence holds
  their ids and results), on Rust 1.98.0.
- The half-close form of this change (`v1.5.0-ra.1`, branch `ra/v1.5.0-client-leave`): the body-streaming tests passed
  5 of 5, its test failed without the change, and the whole library suite passed (1994 passed, 0 failed, 1 ignored, with
  every `OTEL_*` variable unset: three upstream config tests read an OTLP endpoint from the environment).
- The cancelling form, with the tests above, has not been compiled or run when this is written; its evidence is the
  Cloud Build run of the consumer's ad-hoc test build.

## Upstream

- Issue: not filed yet. The project's contribution policy asks that issues be written by a person; the
  report will be filed in the maintainers' own words, and its link added here.
- v1.6.0 carries none of the four changes: its ext_proc code still reads the upstream response body in a detached task
  that nothing stops when the client leaves; the body field is still read whole by `jsonField`; the proxy's own errors are
  still the error's text as `text/plain`; and the picker's chosen pod is still looked up once. What v1.6.0 does change near
  them is listed below. The form an upstream pull request would take is the one this branch already has: v1.6.0 has no
  `DropBody`, and `Body::with_drop_guard` (`crates/http/src/body.rs`) takes its place.

## The rebase onto v1.6.0

The series is the fifteen commits of the final `ra/v1.5.0-ra.3` (head `7a6d131e`, on top of `v1.5.0`), in the same order and
with the same subjects. The first twelve (up to `ra/v1.5.0-ra.3-picker-wait-deadline-2`) were rebased onto `v1.6.0` by hand: the first
four commits and the public-error commit were adapted, the rest applied unchanged. The last three (the deadline note in this file;
the public-error commit that makes the Messages rewrite set the JSON headers and the stale-assignment 503 the envelope; and the
integration tests that an authorization call which times out or cannot be reached is the 503 credit gate envelope) were added
afterwards with the re-sync script, a `git cherry-pick -x` each, all without a conflict. No change was dropped: none of them is
redundant on `v1.6.0`. The map from each original commit to its rebased one is kept with the consumer's evidence.

- ext_proc (`crates/agentgateway/src/http/ext_proc.rs`): v1.6.0 replaced `http::DropBody` with the managed `Body`
  (`crates/http/src/body.rs`), so the token's drop guard is attached with `Body::with_drop_guard` (first commit) and the
  processed body is installed as a content wrapper with `Body::transform_stream` (second commit). The request stream of
  the call is v1.6.0's stream that reports its first poll, with `take_until` on the token on top. v1.6.0 spawns the
  response continuation in two places (it added one for a buffered body that has trailers); both go through the new
  helper `spawn_response_continuation`, which stops when the token is cancelled. v1.6.0 also added a flag that tells a
  clean close of the call from a failure (`cleanly_closed`): the abort path does not set it, so a cancelled call is still
  not taken for a clean close.
- The mock server (`test_helpers/extprocmock.rs`) keeps v1.6.0's `close_stream` hook beside our `on_eof` and
  `on_stream_error` hooks, and v1.6.0's new trailer tests stay in `body_streaming_and_trailers` in front of ours (the
  module has nine tests of upstream's now, where v1.5.0 had four).
- Public errors (`proxy/httpproxy.rs`): v1.6.0 resolves the answer of a failed request in `resolve_response` (which also
  puts the error on the request's CEL context, "cel: expose `proxy.error` attributes"), and the envelope is still written
  once at the end, after the response policies, so both a request failure and a policy failure are covered. The backend
  authentication errors v1.6.0 splits into 500 (local) and 502 (credential provider) keep those statuses and get the public
  sentence of their status. The 429 of a local rate limit keeps v1.6.0's `Retry-After` (the envelope adds one only when the
  answer has none), and the answer of a remote rate limit service is still the service's own.
- The authorization-timeout test (`an_authorization_call_that_times_out_is_a_503_credit_gate_unavailable`) sets the call's own
  `requestTimeout` of 300 ms, so v1.6.0's new 2-second default for an external authorization call does not decide it.
- Three upstream tests of v1.6.0 assert the text of an error the proxy now answers in the public envelope, and were adapted (same
  names, same status checks): the two MCP oversized-body tests (`mcp/mcp_tests.rs`, which expected the configured limit in the 413
  body) and `llm_custom_provider_rejects_unsupported_format_before_upstream_call` (`tests/tests/llm.rs`, which expected the
  conversion error's text in the 400 body). Each now requires the envelope's code and fixed sentence. The Cloud Build run of every test
  target of the crate found them; nothing else of the integration binary fails (317 passed).
- Flaky on the unpatched v1.6.0 too: a few `http::apikey` tests (`test_apikey_sha256_*`, `test_apikey_cookie_*`, and others,
  a different one in different runs) fail in some Cloud Build runs of the library suite whether or not the patch is applied; a rerun
  passes. They are not ours and are not changed.
- Behaviour of v1.6.0 that a consumer should know and this branch does not change: the external authorization call has a
  default timeout of 2 seconds, and (the release notes' words) rate limit and external processing calls default to 10
  seconds; a failure to get a credential from a provider is 502 where it was 500.
- Build: v1.6.0's own `rust-toolchain.toml` names Rust 1.99.0 (its Dockerfile uses `rust:1.99.0-trixie`); the code needs
  only `rust-version = 1.90`, and the crates type-check, test targets included, with Rust 1.98.1.

## Plan

This branch is dropped as soon as an upstream release carries a fix for the issue; the consumer then pins
that release instead.
