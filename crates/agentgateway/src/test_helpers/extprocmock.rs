use std::sync::Arc;

use async_trait::async_trait;
use protos::envoy::service::ext_proc::v3::{BodyMutation, body_mutation};
use tokio::sync::mpsc;
use tokio_stream;
use tonic::{Request, Response as TonicResponse, Status, Streaming};

use crate::http::ext_proc::proto::external_processor_server::{
	ExternalProcessor, ExternalProcessorServer,
};
use crate::http::ext_proc::proto::{
	self, CommonResponse, HttpHeaders, HttpTrailers, ProcessingRequest, ProcessingResponse,
	processing_request, processing_response,
};
use crate::test_helpers::common::MockInstance;
use crate::*;

pub fn request_header_response(cr: Option<CommonResponse>) -> Result<ProcessingResponse, Status> {
	Ok(ProcessingResponse {
		response: Some(processing_response::Response::RequestHeaders(
			proto::HeadersResponse { response: cr },
		)),
		..Default::default()
	})
}

pub fn request_body_response(cr: Option<CommonResponse>) -> Result<ProcessingResponse, Status> {
	Ok(ProcessingResponse {
		response: Some(processing_response::Response::RequestBody(
			proto::BodyResponse { response: cr },
		)),
		..Default::default()
	})
}

pub fn response_header_response(cr: Option<CommonResponse>) -> Result<ProcessingResponse, Status> {
	Ok(ProcessingResponse {
		response: Some(processing_response::Response::ResponseHeaders(
			proto::HeadersResponse { response: cr },
		)),
		..Default::default()
	})
}
pub fn response_body_response(cr: Option<CommonResponse>) -> Result<ProcessingResponse, Status> {
	Ok(ProcessingResponse {
		response: Some(processing_response::Response::ResponseBody(
			proto::BodyResponse { response: cr },
		)),
		..Default::default()
	})
}

pub fn immediate_response(cr: proto::ImmediateResponse) -> Result<ProcessingResponse, Status> {
	Ok(ProcessingResponse {
		response: Some(processing_response::Response::ImmediateResponse(cr)),
		..Default::default()
	})
}

pub fn request_header_response_with_dynamic_metadata(
	cr: Option<CommonResponse>,
	metadata: prost_wkt_types::Struct,
) -> Result<ProcessingResponse, Status> {
	Ok(ProcessingResponse {
		response: Some(processing_response::Response::RequestHeaders(
			proto::HeadersResponse { response: cr },
		)),
		dynamic_metadata: Some(metadata),
		..Default::default()
	})
}

pub fn response_header_response_with_dynamic_metadata(
	cr: Option<CommonResponse>,
	metadata: prost_wkt_types::Struct,
) -> Result<ProcessingResponse, Status> {
	Ok(ProcessingResponse {
		response: Some(processing_response::Response::ResponseHeaders(
			proto::HeadersResponse { response: cr },
		)),
		dynamic_metadata: Some(metadata),
		..Default::default()
	})
}

#[async_trait]
pub trait Handler {
	async fn on_open(&mut self, _metadata: &tonic::metadata::MetadataMap) {}

	async fn on_request(&mut self, _request: &ProcessingRequest) {}

	fn close_stream(&self) -> bool {
		false
	}

	/// Called when the client's stream of requests ends without failing, after the last request.
	///
	/// A tonic server also reports a call the client cancelled (RST_STREAM with CANCEL) this way: its
	/// request stream treats a cancelled body as the end of the stream. To tell the two apart, look at
	/// `response_stream`: the server's connection drops the response stream of a cancelled call at
	/// once, which `closed()` reports, and leaves that of a half-closed call open for the client to read.
	async fn on_eof(&mut self, _response_stream: &mpsc::Sender<Result<ProcessingResponse, Status>>) {}

	/// Called when the stream of requests fails instead of ending, which is what a server sees when the
	/// client cancels the call (the HTTP/2 stream is reset).
	async fn on_stream_error(&mut self, _status: &Status) {}

	async fn handle_request_headers(
		&mut self,
		_headers: &HttpHeaders,
		sender: &mpsc::Sender<Result<ProcessingResponse, Status>>,
	) -> Result<(), Status> {
		let _ = sender.send(request_header_response(None)).await;
		Ok(())
	}

	async fn handle_request_body(
		&mut self,
		b: &proto::HttpBody,
		sender: &mpsc::Sender<Result<ProcessingResponse, Status>>,
	) -> Result<(), Status> {
		let _ = sender
			.send(request_body_response(Some(CommonResponse {
				body_mutation: Some(BodyMutation {
					mutation: Some(body_mutation::Mutation::StreamedResponse(
						proto::StreamedBodyResponse {
							body: b.body.clone(),
							end_of_stream: b.end_of_stream,
						},
					)),
				}),
				..Default::default()
			})))
			.await;
		Ok(())
	}

	async fn handle_response_headers(
		&mut self,
		_headers: &HttpHeaders,
		sender: &mpsc::Sender<Result<ProcessingResponse, Status>>,
	) -> Result<(), Status> {
		let _ = sender.send(response_header_response(None)).await;
		Ok(())
	}

	async fn handle_response_body(
		&mut self,
		b: &proto::HttpBody,
		sender: &mpsc::Sender<Result<ProcessingResponse, Status>>,
	) -> Result<(), Status> {
		let _ = sender
			.send(response_body_response(Some(CommonResponse {
				body_mutation: Some(BodyMutation {
					mutation: Some(body_mutation::Mutation::StreamedResponse(
						proto::StreamedBodyResponse {
							body: b.body.clone(),
							end_of_stream: b.end_of_stream,
						},
					)),
				}),
				..Default::default()
			})))
			.await;
		Ok(())
	}

	async fn handle_request_trailers(
		&mut self,
		_trailers: &HttpTrailers,
		_sender: &mpsc::Sender<Result<ProcessingResponse, Status>>,
	) -> Result<(), Status> {
		Ok(())
	}

	async fn handle_response_trailers(
		&mut self,
		_trailers: &HttpTrailers,
		_sender: &mpsc::Sender<Result<ProcessingResponse, Status>>,
	) -> Result<(), Status> {
		Ok(())
	}
}

pub struct ReplaceRequestBody(pub bytes::Bytes);

#[async_trait]
impl Handler for ReplaceRequestBody {
	async fn handle_request_body(
		&mut self,
		_body: &proto::HttpBody,
		sender: &mpsc::Sender<Result<ProcessingResponse, Status>>,
	) -> Result<(), Status> {
		let response = CommonResponse {
			body_mutation: Some(BodyMutation {
				mutation: Some(body_mutation::Mutation::Body(self.0.clone())),
			}),
			..Default::default()
		};
		let _ = sender.send(request_body_response(Some(response))).await;
		Ok(())
	}
}

/// Mock ext_proc server for testing
pub struct ExtProcMock<T> {
	handler: Arc<dyn Fn() -> T + Send + Sync + 'static>,
}

impl<T> Clone for ExtProcMock<T> {
	fn clone(&self) -> Self {
		Self {
			handler: self.handler.clone(),
		}
	}
}

impl<T> ExtProcMock<T>
where
	T: Handler + Send + Sync + 'static,
{
	/// Create a new mock with default configuration
	pub fn new(handler: impl Fn() -> T + Send + Sync + 'static) -> Self {
		Self {
			handler: Arc::new(handler),
		}
	}

	pub async fn spawn(&self) -> MockInstance {
		let srv = ExternalProcessorServer::new(self.clone());
		super::common::spawn_service(srv).await
	}

	pub async fn spawn_on(&self, address: std::net::SocketAddr) -> MockInstance {
		let srv = ExternalProcessorServer::new(self.clone());
		super::common::spawn_service_on(srv, address).await
	}
}

#[tonic::async_trait]
impl<T> ExternalProcessor for ExtProcMock<T>
where
	T: Handler + Send + Sync + 'static,
{
	type ProcessStream = tokio_stream::wrappers::ReceiverStream<Result<ProcessingResponse, Status>>;

	async fn process(
		&self,
		request: Request<Streaming<ProcessingRequest>>,
	) -> Result<TonicResponse<Self::ProcessStream>, Status> {
		let (tx, rx) = mpsc::channel(32);

		let mut handler = (self.handler.clone())();
		handler.on_open(request.metadata()).await;

		tokio::spawn(async move {
			let result = async {
				let mut request_stream = request.into_inner();

				loop {
					let request_result = match request_stream.message().await {
						Ok(Some(request_result)) => request_result,
						Ok(None) => break,
						Err(status) => {
							handler.on_stream_error(&status).await;
							return Err(status);
						},
					};
					trace!("Received request: {:?}", request_result.request);
					handler.on_request(&request_result).await;
					match request_result.request {
						Some(processing_request::Request::RequestHeaders(headers)) => {
							handler.handle_request_headers(&headers, &tx).await?;
						},
						Some(processing_request::Request::RequestBody(body)) => {
							handler.handle_request_body(&body, &tx).await?;
						},
						Some(processing_request::Request::ResponseHeaders(headers)) => {
							handler.handle_response_headers(&headers, &tx).await?;
						},
						Some(processing_request::Request::ResponseBody(body)) => {
							handler.handle_response_body(&body, &tx).await?;
						},
						Some(processing_request::Request::RequestTrailers(trailers)) => {
							handler.handle_request_trailers(&trailers, &tx).await?;
						},
						Some(processing_request::Request::ResponseTrailers(trailers)) => {
							handler.handle_response_trailers(&trailers, &tx).await?;
						},
						None => {
							// Invalid request
							continue;
						},
					}
					if handler.close_stream() {
						return Ok::<(), Status>(());
					}
				}
				handler.on_eof(&tx).await;
				Ok::<(), Status>(())
			}
			.await;
			if let Err(status) = result {
				let _ = tx.send(Err(status)).await;
			}
		});

		Ok(TonicResponse::new(
			tokio_stream::wrappers::ReceiverStream::new(rx),
		))
	}
}
