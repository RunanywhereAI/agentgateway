pub mod ateapimock;
pub mod credprovidermock;
pub mod extauthmock;
pub mod extmcpmock;
pub mod extprocmock;
mod hyper_tower;
pub mod oteltracemock;
#[cfg(any(test, feature = "internal_benches"))]
mod policy;
pub mod proxymock;
pub mod ratelimitmock;
pub use common::MockInstance;
#[cfg(test)]
pub(crate) use common::spawn_service;
#[cfg(all(test, target_os = "linux"))]
pub(crate) use common::{wedgeable_relay, wedged_peer};
#[cfg(any(test, feature = "internal_benches"))]
pub use policy::{policy_client, test_policy};

mod common {
	use std::net::SocketAddr;

	use hyper::server::conn::http2;
	use tokio::task::JoinHandle;
	use tonic::body::Body;
	use tower::BoxError;
	use tracing::error;

	pub struct MockInstance {
		pub address: SocketAddr,
		handle: JoinHandle<()>,
	}

	impl Drop for MockInstance {
		fn drop(&mut self) {
			self.handle.abort();
		}
	}

	pub async fn spawn_service<S>(srv: S) -> MockInstance
	where
		S: tower::Service<hyper::Request<Body>, Response = http::Response<Body>>
			+ Clone
			+ Send
			+ Sync
			+ 'static,
		S::Future: Send + 'static,
		S::Error: Into<BoxError> + 'static,
	{
		let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
		let addr = listener.local_addr().unwrap();
		let task = tokio::spawn(async move {
			while let Ok((socket, _)) = listener.accept().await {
				let srv = srv.clone();
				tokio::spawn(async move {
					if let Err(err) = http2::Builder::new(::hyper_util::rt::TokioExecutor::new())
						.serve_connection(
							hyper_util::rt::TokioIo::new(socket),
							super::hyper_tower::TowerToHyperService::new(srv),
						)
						.await
					{
						error!("Error serving connection: {:?}", err);
					}
				});
			}
		});
		MockInstance {
			address: addr,
			handle: task,
		}
	}

	/// A TCP peer that accepts one connection and then neither reads from it nor writes to it, with
	/// the smallest receive buffer the kernel allows (the accepted socket inherits it from the
	/// listener). After a few KB it offers a window of zero: what a sender writes past that stays
	/// unsent in its own kernel, and the sender's kernel can only probe the window, which the peer's
	/// kernel keeps answering. It is how a test makes a connection that has data to send and no
	/// progress, without dropping packets (which needs privileges a test does not have).
	#[cfg(all(test, target_os = "linux"))]
	pub async fn wedged_peer() -> MockInstance {
		use socket2::{Domain, Socket, Type};

		let socket = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
		socket.set_recv_buffer_size(1).unwrap();
		socket
			.bind(&SocketAddr::from(([127, 0, 0, 1], 0)).into())
			.unwrap();
		socket.listen(8).unwrap();
		socket.set_nonblocking(true).unwrap();
		let listener = tokio::net::TcpListener::from_std(socket.into()).unwrap();
		let address = listener.local_addr().unwrap();
		let handle = tokio::spawn(async move {
			let (connection, _) = listener.accept().await.unwrap();
			std::future::pending::<()>().await;
			drop(connection);
		});
		MockInstance { address, handle }
	}

	/// A TCP relay in front of `target` that can be wedged in the middle of a connection. It relays one
	/// connection both ways, with the smallest receive buffer the kernel allows on the side the caller
	/// connects to. Once `wedge` is set it stops reading from that side and stops relaying, but keeps
	/// the connection open and acknowledging probes: what the caller sends from then on stays unsent in
	/// its own kernel (`wedged_peer`'s case, but for a connection that was healthy until then).
	#[cfg(all(test, target_os = "linux"))]
	pub async fn wedgeable_relay(
		target: SocketAddr,
		wedge: std::sync::Arc<std::sync::atomic::AtomicBool>,
	) -> MockInstance {
		use std::sync::atomic::Ordering;

		use socket2::{Domain, Socket, Type};
		use tokio::io::{AsyncReadExt, AsyncWriteExt};

		let socket = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
		socket.set_recv_buffer_size(1).unwrap();
		socket
			.bind(&SocketAddr::from(([127, 0, 0, 1], 0)).into())
			.unwrap();
		socket.listen(8).unwrap();
		socket.set_nonblocking(true).unwrap();
		let listener = tokio::net::TcpListener::from_std(socket.into()).unwrap();
		let address = listener.local_addr().unwrap();
		let handle = tokio::spawn(async move {
			let (caller, _) = listener.accept().await.unwrap();
			let upstream = tokio::net::TcpStream::connect(target).await.unwrap();
			let (mut caller_read, mut caller_write) = caller.into_split();
			let (mut upstream_read, mut upstream_write) = upstream.into_split();
			let wedged = wedge.clone();
			let forward = tokio::spawn(async move {
				let mut buffer = [0u8; 4096];
				loop {
					let n = match caller_read.read(&mut buffer).await {
						Ok(0) | Err(_) => return,
						Ok(n) => n,
					};
					if wedged.load(Ordering::SeqCst) {
						// Reading stops here and the connection stays open.
						std::future::pending::<()>().await;
					}
					if upstream_write.write_all(&buffer[..n]).await.is_err() {
						return;
					}
				}
			});
			let mut buffer = [0u8; 4096];
			loop {
				let n = match upstream_read.read(&mut buffer).await {
					Ok(0) | Err(_) => break,
					Ok(n) => n,
				};
				if wedge.load(Ordering::SeqCst) {
					std::future::pending::<()>().await;
				}
				if caller_write.write_all(&buffer[..n]).await.is_err() {
					break;
				}
			}
			forward.abort();
		});
		MockInstance { address, handle }
	}

	pub async fn spawn_service_on<S>(srv: S, address: SocketAddr) -> MockInstance
	where
		S: tower::Service<hyper::Request<Body>, Response = http::Response<Body>>
			+ Clone
			+ Send
			+ Sync
			+ 'static,
		S::Future: Send + 'static,
		S::Error: Into<BoxError> + 'static,
	{
		let listener = tokio::net::TcpListener::bind(address).await.unwrap();
		let addr = listener.local_addr().unwrap();
		let task = tokio::spawn(async move {
			while let Ok((socket, _)) = listener.accept().await {
				let srv = srv.clone();
				tokio::spawn(async move {
					if let Err(err) = http2::Builder::new(::hyper_util::rt::TokioExecutor::new())
						.serve_connection(
							hyper_util::rt::TokioIo::new(socket),
							super::hyper_tower::TowerToHyperService::new(srv),
						)
						.await
					{
						error!("Error serving connection: {:?}", err);
					}
				});
			}
		});
		MockInstance {
			address: addr,
			handle: task,
		}
	}
}

#[cfg(test)]
mod tests {
	use opentelemetry_proto::tonic::collector::trace::v1::{
		ExportTraceServiceRequest, ExportTraceServiceResponse,
	};
	use protos::envoy::service::auth::v3::{CheckRequest, CheckResponse};
	use tonic::Status;

	use super::{extauthmock, extmcpmock, extprocmock, oteltracemock, ratelimitmock};
	use crate::test_helpers::extauthmock::allow_response;
	use crate::test_helpers::oteltracemock::ok_response;

	struct DevExtProcHandler;

	#[async_trait::async_trait]
	impl extprocmock::Handler for DevExtProcHandler {}

	struct DevExtMcpHandler;

	#[async_trait::async_trait]
	impl extmcpmock::Handler for DevExtMcpHandler {}

	struct DevRateLimitHandler;

	#[async_trait::async_trait]
	impl ratelimitmock::Handler for DevRateLimitHandler {}

	struct DevExtAuthHandler;

	#[async_trait::async_trait]
	impl extauthmock::Handler for DevExtAuthHandler {
		async fn check(&mut self, request: &CheckRequest) -> Result<CheckResponse, Status> {
			tracing::info!("got extauth request {:#?}", request);
			allow_response(None)
		}
	}

	struct DevOtelTraceHandler;

	#[async_trait::async_trait]
	impl oteltracemock::Handler for DevOtelTraceHandler {
		async fn export(
			&mut self,
			_request: &ExportTraceServiceRequest,
		) -> Result<ExportTraceServiceResponse, Status> {
			tracing::info!("got trace request");
			ok_response()
		}
	}

	// Run with: cargo test --lib -p agentgateway -- --ignored start_dev_mocks_on_fixed_ports --nocapture
	#[tokio::test]
	#[ignore = "dev helper: starts mock services on fixed ports and hangs"]
	async fn start_dev_mocks_on_fixed_ports() {
		agent_core::telemetry::testing::setup_test_logging();
		let ext_proc = extprocmock::ExtProcMock::new(|| DevExtProcHandler)
			.spawn_on(([127, 0, 0, 1], 9995).into())
			.await;
		tracing::info!("ext_proc mock started on {}", ext_proc.address);

		let rate_limit = ratelimitmock::RateLimitMock::new(|| DevRateLimitHandler)
			.spawn_on(([127, 0, 0, 1], 9996).into())
			.await;
		tracing::info!("ratelimit mock started on {}", rate_limit.address);

		let ext_auth = extauthmock::ExtAuthMock::new(|| DevExtAuthHandler)
			.spawn_on(([127, 0, 0, 1], 9997).into())
			.await;
		tracing::info!("ext_auth mock started on {}", ext_auth.address);

		let ext_mcp = extmcpmock::ExtMcpMock::new(|| DevExtMcpHandler)
			.spawn_on(([127, 0, 0, 1], 9999).into())
			.await;
		tracing::info!("ext_mcp mock started on {}", ext_mcp.address);

		let otel_trace = oteltracemock::OtelTraceMock::new(|| DevOtelTraceHandler)
			.spawn_on(([127, 0, 0, 1], 9998).into())
			.await;
		tracing::info!("otel trace mock started on {}", otel_trace.address);

		let _instances = (ext_proc, rate_limit, ext_auth, ext_mcp, otel_trace);
		std::future::pending::<()>().await;
	}
}
