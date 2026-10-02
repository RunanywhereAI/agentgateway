# RunanywhereAI fork of agentgateway

This branch, `ra/v1.5.0-client-leave`, is upstream tag `v1.5.0`
(commit fe6732474a96a0363dfb9822859af4e9bab360fa) plus one change and its test. Nothing else differs.

## What it carries

When an ext_proc policy processes the response body in `FULL_DUPLEX_STREAMED` mode and the downstream
client disconnects after the response headers were sent, upstream v1.5.0 keeps reading the upstream body
to its end in a detached task, and the ext_proc stream keeps receiving every chunk. This change ties that
background work to ownership of the processed response body: a per-instance cancellation token, cancelled
when the returned body is dropped (through the crate's own `http::DropBody`), aborts the upstream-body
reader (dropping the upstream body, which cancels the upstream request), ends the gRPC request stream to
the ext_proc server (`take_until` on the token) and stops the response continuation.

Files: `crates/agentgateway/src/http/ext_proc.rs` (the fix),
`crates/agentgateway/src/http/ext_proc_tests.rs` (the test
`dropping_the_response_cancels_the_upstream_and_ends_the_ext_proc_stream`) and
`crates/agentgateway/src/test_helpers/extprocmock.rs` (an `on_eof` hook on the mock handler).

## Why

Behind this gateway an inference engine keeps generating for a client that has gone, and the metering
ext_proc server charges for tokens the client never received.

## Evidence

- With the change, `cargo test --locked -p agentgateway --lib -- http::ext_proc::tests::body_streaming_and_trailers`
  passes 5 of 5, the new test among them.
- Without the change (the test alone), the new test fails after its 2-second timeout and the other four pass.
- The whole library suite with the change: 1994 passed, 0 failed, 1 ignored (`cargo test --locked -p agentgateway --lib`,
  with every `OTEL_*` variable unset: three upstream config tests read an OTLP endpoint from the environment).

## Upstream

- Issue: not filed yet. The project's contribution policy asks that issues be written by a person; the
  report will be filed in the maintainers' own words, and its link added here.
- This change does not apply cleanly on upstream `main` (c7ac602e, 310 commits after v1.5.0): the ext_proc
  code moved. On `main` the crate's `DropBody` is gone and `Body::with_drop_guard` (`crates/http/src/body.rs`) takes
  its place, so a port to `main` is the form an upstream pull request would take.

## Plan

This branch is dropped as soon as an upstream release carries a fix for the issue; the consumer then pins
that release instead.
