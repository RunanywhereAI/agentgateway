use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body::{Frame, SizeHint};
use tokio_util::sync::{CancellationToken, WaitForCancellationFutureOwned};

use crate::http;
use crate::proxy::ProxyError;
use crate::proxy::httpproxy::PolicyClient;
use crate::types::agent::{BackendTrafficPolicy, SimpleBackendReference};

#[derive(Clone, Debug)]
pub struct GrpcReferenceChannel {
	pub target: Arc<SimpleBackendReference>,
	pub client: PolicyClient,
	pub policies: Arc<Vec<BackendTrafficPolicy>>,
}

impl tower::Service<::http::Request<tonic::body::Body>> for GrpcReferenceChannel {
	type Response = http::Response;
	type Error = ProxyError;
	type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

	fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
		Ok(()).into()
	}

	fn call(&mut self, req: ::http::Request<tonic::body::Body>) -> Self::Future {
		let client = self.client.clone();
		let target = self.target.clone();
		let policies = self.policies.clone();
		let req = req.map(http::Body::new);
		Box::pin(async move {
			client
				.call_reference_with_policies_untraced(req, &target, policies.as_slice())
				.await
		})
	}
}

/// `GrpcReferenceChannel`, whose calls can be cancelled from outside. Until `abort` is cancelled its
/// calls are the inner channel's. Once it is, the request body of every call still being sent fails
/// with a cancellation, so the HTTP/2 stream of that call is reset with CANCEL: the server sees a
/// cancelled call, where ending the stream of requests would show it a clean end.
#[derive(Clone, Debug)]
pub struct AbortableChannel {
	pub inner: GrpcReferenceChannel,
	pub abort: CancellationToken,
}

impl tower::Service<::http::Request<tonic::body::Body>> for AbortableChannel {
	type Response = http::Response;
	type Error = ProxyError;
	type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

	fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
		<GrpcReferenceChannel as tower::Service<::http::Request<tonic::body::Body>>>::poll_ready(
			&mut self.inner,
			cx,
		)
	}

	fn call(&mut self, req: ::http::Request<tonic::body::Body>) -> Self::Future {
		let client = self.inner.client.clone();
		let target = self.inner.target.clone();
		let policies = self.inner.policies.clone();
		let abort = self.abort.clone();
		let req = req.map(|body| http::Body::new(CancelOnAbort::new(http::Body::new(body), abort)));
		// The call of the stream to an external processor: a processor that falls silent without its
		// connection closing must end the stream in seconds (see `with_dead_peer_keepalive`).
		Box::pin(async move {
			client
				.call_reference_with_policies_untraced(req, &target, policies.as_slice())
				.await
		})
	}
}

/// A request body that fails with a cancellation (see `agent_hbone::cancelled`) as soon as `abort`
/// is cancelled, before it hands on anything else, and is otherwise the body it wraps. It also fails
/// instead of ending when `abort` was cancelled while it was reading the end of the wrapped body, so
/// that an abort is never answered with a clean end of the stream.
struct CancelOnAbort {
	body: http::Body,
	token: CancellationToken,
	abort: Pin<Box<WaitForCancellationFutureOwned>>,
}

impl CancelOnAbort {
	fn new(body: http::Body, token: CancellationToken) -> Self {
		Self {
			body,
			abort: Box::pin(token.clone().cancelled_owned()),
			token,
		}
	}

	fn cancelled() -> <Self as http_body::Body>::Error {
		Box::new(agent_hbone::cancelled())
	}
}

impl http_body::Body for CancelOnAbort {
	type Data = Bytes;
	type Error = Box<dyn std::error::Error + Send + Sync>;

	fn poll_frame(
		mut self: Pin<&mut Self>,
		cx: &mut Context<'_>,
	) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
		if self.abort.as_mut().poll(cx).is_ready() {
			return Poll::Ready(Some(Err(Self::cancelled())));
		}
		match Pin::new(&mut self.body).poll_frame(cx) {
			Poll::Ready(None) if self.token.is_cancelled() => Poll::Ready(Some(Err(Self::cancelled()))),
			polled => polled
				.map(|frame| frame.map(|frame| frame.map_err(|error| -> Self::Error { Box::new(error) }))),
		}
	}

	fn is_end_stream(&self) -> bool {
		self.body.is_end_stream()
	}

	fn size_hint(&self) -> SizeHint {
		self.body.size_hint()
	}
}
