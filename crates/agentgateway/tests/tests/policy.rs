use agentgateway::test_helpers::extauthmock;

use crate::common::prelude::*;

/// The body of a response as JSON. `read_body!` reads at most 100 bytes, which an error object passes.
async fn json_body(res: Response) -> serde_json::Value {
	let bytes = agentgateway::http::read_body_with_limit(res.into_body(), 4096)
		.await
		.unwrap();
	serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn response_policy_short_circuit() {
	let (_mock, mut bind, io) = basic_setup().await;
	bind
		.attach_route(json!({
			"policies": {
				"extAuthz": {
					// Dummy host that should fail
					"host": "127.0.0.1:1",
				},
				"responseHeaderModifier": {
					"add": {
						"x-filter": "x-filter-val"
					},
				},
				"transformations": {
					"response": {
						"add": {
							"x-xfm": "'x-xfm-val'",
						},
					},
				},
			},
		}))
		.await;

	let res = send_request(io.clone(), Method::GET, "http://lo/p").await;
	// The authorization service gave no answer and the policy fails closed: the client is told the
	// service is unavailable (the public `credit_gate_unavailable`), not that it is not entitled.
	assert_eq!(res.status(), 503);
	// Each type of response modifier should NOT run since the ext_authz short-circuits the req
	assert_eq!(res.hdr("x-filter"), "");
	assert_eq!(res.hdr("x-xfm"), "");
	assert_eq!(res.hdr("content-type"), "application/json");
	assert_eq!(res.hdr("retry-after"), "10");
	let body = json_body(res).await;
	assert_eq!(body["error"]["code"], "credit_gate_unavailable");
	assert_eq!(body["error"]["type"], "server_error");
	assert!(!body.to_string().contains("127.0.0.1"));
}

#[tokio::test]
async fn an_error_the_proxy_makes_on_a_messages_path_is_the_messages_envelope() {
	let (_mock, mut bind, io) = basic_setup().await;
	bind
		.attach_route(json!({
			"policies": {
				"extAuthz": {
					// Dummy host that should fail
					"host": "127.0.0.1:1",
				},
			},
		}))
		.await;

	let res = send_request(io.clone(), Method::POST, "http://lo/v1/messages").await;
	assert_eq!(res.status(), 503);
	assert_eq!(res.hdr("content-type"), "application/json");
	assert_eq!(res.hdr("retry-after"), "10");
	let body = json_body(res).await;
	assert_eq!(body["type"], "error");
	assert_eq!(body["error"]["type"], "api_error");
	assert_eq!(body["request_id"], "unknown");

	let res = send_request(io, Method::POST, "http://lo/v1/chat/completions").await;
	let body = json_body(res).await;
	assert!(body.get("type").is_none());
	assert_eq!(body["error"]["code"], "credit_gate_unavailable");
}

#[tokio::test]
async fn header_manipulation() {
	let (mock, mut bind, _io) = basic_setup().await;
	bind
		.attach_route(json!({
			"policies": {
				"requestHeaderModifier": {
					"add": {
						"x-route-req": "route-req",
					},
				},
				"responseHeaderModifier": {
					"add": {
						"x-route-resp": "route-resp",
					},
				},
			},
			"backends": [{
				"host": mock.address().to_string(),
				"policies": {
					"requestHeaderModifier": {
						"add": {
							"x-backend-req": "backend-req",
						},
					},
					"responseHeaderModifier": {
						"add": {
							"x-backend-resp": "backend-resp",
						},
					},
					"transformations": {
						"request": {
							"set": {
								"x-backend-xfm-req": "'backend-xfm-req'",
							},
						},
						"response": {
							"add": {
								"x-backend-xfm-resp": "'backend-xfm-resp'",
							},
						},
					},
				},
			}],
		}))
		.await;
	let io = bind.serve_http(BIND_KEY);

	let res = send_request(io.clone(), Method::GET, "http://lo/p").await;
	assert_eq!(res.status(), 200);
	assert_eq!(res.hdr("x-route-resp"), "route-resp");
	assert_eq!(res.hdr("x-backend-resp"), "backend-resp");
	assert_eq!(res.hdr("x-backend-xfm-resp"), "backend-xfm-resp");
	let body = read_body(res.into_body()).await;
	assert_eq!(
		body.headers.get("x-route-req").unwrap().as_bytes(),
		b"route-req"
	);
	assert_eq!(
		body.headers.get("x-backend-req").unwrap().as_bytes(),
		b"backend-req"
	);
	assert_eq!(
		body.headers.get("x-backend-xfm-req").unwrap().as_bytes(),
		b"backend-xfm-req"
	);
}

/// The public 503 an authorization call that gets no answer is made into: the credit gate envelope with a
/// Retry-After, nothing of the failure in it.
async fn assert_credit_gate_unavailable(res: Response) {
	assert_eq!(res.status(), 503);
	assert_eq!(res.hdr("content-type"), "application/json");
	assert_eq!(res.hdr("retry-after"), "10");
	let body = json_body(res).await;
	assert_eq!(body["error"]["code"], "credit_gate_unavailable", "{body}");
	assert_eq!(body["error"]["type"], "server_error", "{body}");
	assert!(body["error"]["param"].is_null(), "{body}");
	let text = body.to_string();
	for internal in [
		"timed out",
		"deadline",
		"refused",
		"127.0.0.1",
		"ext_authz",
		"authorization",
	] {
		assert!(!text.contains(internal), "{text}");
	}
}

#[tokio::test]
async fn an_authorization_call_that_times_out_is_a_503_credit_gate_unavailable() {
	struct Slow;

	#[async_trait::async_trait]
	impl extauthmock::Handler for Slow {
		async fn check(
			&mut self,
			_request: &agentgateway::http::ext_authz::proto::CheckRequest,
		) -> Result<agentgateway::http::ext_authz::proto::CheckResponse, tonic::Status> {
			tokio::time::sleep(Duration::from_secs(10)).await;
			extauthmock::allow_response(None)
		}
	}

	let (_mock, mut bind, io) = basic_setup().await;
	let authz = extauthmock::ExtAuthMock::new(|| Slow).spawn().await;
	bind
		.attach_route(json!({
			"policies": {
				"extAuthz": {
					"host": authz.address.to_string(),
					"policies": {"http": {"requestTimeout": "300ms"}},
				},
			},
		}))
		.await;

	let began = std::time::Instant::now();
	let res = send_request(io, Method::POST, "http://lo/v1/chat/completions").await;
	assert!(
		began.elapsed() < Duration::from_secs(5),
		"the call was not cut at its timeout"
	);
	assert_credit_gate_unavailable(res).await;
}

#[tokio::test]
async fn an_authorization_service_that_cannot_be_reached_is_a_503_credit_gate_unavailable() {
	let (_mock, mut bind, io) = basic_setup().await;
	bind
		.attach_route(json!({
			"policies": {
				"extAuthz": {
					// Nothing listens here: the connection is refused, as with no admission backend at all.
					"host": "127.0.0.1:1",
				},
			},
		}))
		.await;

	assert_credit_gate_unavailable(
		send_request(io, Method::POST, "http://lo/v1/chat/completions").await,
	)
	.await;
}

#[tokio::test]
async fn an_authorization_service_that_refuses_without_a_body_is_a_403_in_the_envelope() {
	// The residual case: the service answers with a denial and no HTTP response. The proxy cannot tell this
	// from a refusal it should pass on, so it stays a 403, with the public sentence and no code.
	struct DeniesWithoutBody;

	#[async_trait::async_trait]
	impl extauthmock::Handler for DeniesWithoutBody {
		async fn check(
			&mut self,
			_request: &agentgateway::http::ext_authz::proto::CheckRequest,
		) -> Result<agentgateway::http::ext_authz::proto::CheckResponse, tonic::Status> {
			Ok(agentgateway::http::ext_authz::proto::CheckResponse {
				status: Some(agentgateway::http::ext_authz::proto::Status {
					code: 14,
					message: "unavailable".to_string(),
					details: vec![],
				}),
				http_response: None,
				dynamic_metadata: None,
			})
		}
	}

	let (_mock, mut bind, io) = basic_setup().await;
	let authz = extauthmock::ExtAuthMock::new(|| DeniesWithoutBody)
		.spawn()
		.await;
	bind
		.attach_route(json!({
			"policies": {"extAuthz": {"host": authz.address.to_string()}},
		}))
		.await;

	let res = send_request(io, Method::POST, "http://lo/v1/chat/completions").await;
	assert_eq!(res.status(), 403);
	assert_eq!(res.hdr("content-type"), "application/json");
	let body = json_body(res).await;
	assert_eq!(body["error"]["type"], "permission_error", "{body}");
	assert!(body["error"]["code"].is_null(), "{body}");
	assert!(!body.to_string().contains("unavailable"), "{body}");
}

#[tokio::test]
async fn gateway_ext_authz_response_headers_are_preserved() {
	struct AddResponseHeader;

	#[async_trait::async_trait]
	impl extauthmock::Handler for AddResponseHeader {
		async fn check(
			&mut self,
			_request: &agentgateway::http::ext_authz::proto::CheckRequest,
		) -> Result<agentgateway::http::ext_authz::proto::CheckResponse, tonic::Status> {
			use agentgateway::http::ext_authz::proto::check_response::HttpResponse;
			use agentgateway::http::ext_authz::proto::{HeaderValue, HeaderValueOption, OkHttpResponse};

			extauthmock::allow_response(Some(HttpResponse::OkResponse(OkHttpResponse {
				headers: vec![],
				headers_to_remove: vec![],
				response_headers_to_add: vec![HeaderValueOption {
					header: Some(HeaderValue {
						key: "x-gateway-authz-response".to_string(),
						value: "allowed".to_string(),
						raw_value: vec![],
					}),
					append: Some(false),
					append_action: 0,
				}],
				query_parameters_to_set: vec![],
				query_parameters_to_remove: vec![],
				..Default::default()
			})))
		}
	}

	let (mock, mut bind, io) = basic_setup().await;
	let authz = extauthmock::ExtAuthMock::new(|| AddResponseHeader)
		.spawn()
		.await;
	bind
		.attach_gateway_policy(json!({
			"extAuthz": {
				"host": authz.address,
			},
		}))
		.await;

	let res = send_request(io.clone(), Method::GET, "http://lo/p").await;
	assert_eq!(res.status(), 200);
	assert_eq!(res.hdr("x-gateway-authz-response"), "allowed");
	assert_eq!(read_body(res.into_body()).await.method, Method::GET);
	drop(mock);
}

#[tokio::test]
async fn gateway_http_ext_authz_caches_unauthorized_response() {
	let (_mock, mut bind, io) = basic_setup().await;
	let authz = MockServer::start().await;
	let calls = Arc::new(AtomicUsize::new(0));
	let calls_clone = calls.clone();
	Mock::given(wiremock::matchers::any())
		.respond_with(move |_req: &wiremock::Request| {
			let call = calls_clone.fetch_add(1, Ordering::SeqCst) + 1;
			ResponseTemplate::new(StatusCode::UNAUTHORIZED.as_u16())
				.set_body_string(format!("authz-denied-{call}"))
		})
		.mount(&authz)
		.await;

	bind
		.attach_gateway_policy(json!({
			"extAuthz": {
				"host": authz.address().to_string(),
				"protocol": {"http": {}},
				"cache": {
					"key": ["request.path"],
					"ttl": "60s",
				},
			},
		}))
		.await;

	let res = send_request(io.clone(), Method::GET, "http://lo/p").await;
	assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
	assert_eq!(
		read_body_raw(res.into_body()).await.as_ref(),
		b"authz-denied-1"
	);

	let res = send_request(io.clone(), Method::GET, "http://lo/p").await;
	assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
	assert_eq!(
		read_body_raw(res.into_body()).await.as_ref(),
		b"authz-denied-1"
	);
	assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn gateway_http_ext_authz_does_not_cache_server_error_response() {
	let (_mock, mut bind, io) = basic_setup().await;
	let authz = MockServer::start().await;
	let calls = Arc::new(AtomicUsize::new(0));
	let calls_clone = calls.clone();
	Mock::given(wiremock::matchers::any())
		.respond_with(move |_req: &wiremock::Request| {
			let call = calls_clone.fetch_add(1, Ordering::SeqCst) + 1;
			ResponseTemplate::new(StatusCode::INTERNAL_SERVER_ERROR.as_u16())
				.set_body_string(format!("authz-error-{call}"))
		})
		.mount(&authz)
		.await;

	bind
		.attach_gateway_policy(json!({
			"extAuthz": {
				"host": authz.address().to_string(),
				"protocol": {"http": {}},
				"cache": {
					"key": ["request.path"],
					"ttl": "60s",
				},
			},
		}))
		.await;

	let res = send_request(io.clone(), Method::GET, "http://lo/p").await;
	assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
	assert_eq!(
		read_body_raw(res.into_body()).await.as_ref(),
		b"authz-error-1"
	);

	let res = send_request(io.clone(), Method::GET, "http://lo/p").await;
	assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
	assert_eq!(
		read_body_raw(res.into_body()).await.as_ref(),
		b"authz-error-2"
	);
	assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn gateway_http_ext_authz_body_expression_sets_auth_request_body() {
	let (_mock, mut bind, io) = basic_setup().await;
	let authz = MockServer::start().await;
	Mock::given(wiremock::matchers::any())
		.respond_with(move |req: &wiremock::Request| {
			ResponseTemplate::new(StatusCode::OK.as_u16()).insert_header(
				"x-authz-body",
				String::from_utf8(req.body.clone()).expect("authz request body is utf8"),
			)
		})
		.mount(&authz)
		.await;

	bind
		.attach_gateway_policy(json!({
			"extAuthz": {
				"host": authz.address().to_string(),
				"protocol": {
					"http": {
						"body": r#"{"path": request.path, "method": request.method}"#,
						"includeResponseHeaders": ["x-authz-body"],
					},
				},
			},
		}))
		.await;

	let res = send_request_body(io.clone(), Method::POST, "http://lo/p", b"original").await;
	assert_eq!(res.status(), StatusCode::OK);
	let body = read_body(res.into_body()).await;
	let authz_body: serde_json::Value =
		serde_json::from_slice(body.headers.get("x-authz-body").unwrap().as_bytes()).unwrap();
	assert_eq!(
		authz_body,
		json!({
			"path": "/p",
			"method": "POST",
		})
	);
	assert_eq!(body.body.as_ref(), b"original");
}

#[tokio::test]
async fn gateway_transformation_response_headers_are_applied() {
	let (_mock, mut bind, io) = basic_setup().await;
	bind
		.attach_gateway_policy(json!({
			"transformations": {
				"request": {
					"set": {
						"x-gateway-xfm-req": "'gateway-request'",
					},
				},
				"response": {
					"add": {
						"x-gateway-xfm-resp": "'gateway-response'",
					},
				},
			},
		}))
		.await;

	let res = send_request(io.clone(), Method::GET, "http://lo/p").await;
	assert_eq!(res.status(), 200);
	assert_eq!(res.hdr("x-gateway-xfm-resp"), "gateway-response");
	let body = read_body(res.into_body()).await;
	assert_eq!(
		body.headers.get("x-gateway-xfm-req").unwrap().as_bytes(),
		b"gateway-request"
	);
}

#[tokio::test]
async fn inline_backend_policies() {
	let (mock, mut bind, io) = basic_setup().await;
	bind
		.attach_backend(json!({
			"name": "backend1",
			"host": mock.address(),
			"policies": {
				"requestHeaderModifier": {
					"add": {
						"x-backend-req": "backend-req",
					}
				},
				"responseHeaderModifier": {
					"add": {
						"x-backend-resp": "backend-resp",
					}
				}
			}
		}))
		.await;
	bind
		.attach_route(json!({
			"policies": {
				"requestHeaderModifier": {
					"add": {
						"x-route-req": "route-req",
					},
				},
				"responseHeaderModifier": {
					"add": {
						"x-route-resp": "route-resp",
					},
				},
			},
			"backends": [{
				"backend": "/backend1",
				"policies": {
					"requestHeaderModifier": {
						"add": {
							"x-backend-route-req": "backend-route-req",
						},
					},
					"responseHeaderModifier": {
						"add": {
							"x-backend-route-resp": "backend-route-resp",
						},
					},
				},
			}],
		}))
		.await;

	let res = send_request(io.clone(), Method::GET, "http://lo/p").await;
	assert_eq!(res.status(), 200);
	// We should get the route rule, and the inline backend rule. The Backend rule takes precedence
	// over the HTTPRoute.backendRef.filters though, so that one is ignored (no deep merging, either).
	assert_eq!(res.hdr("x-route-resp"), "route-resp");
	assert_eq!(res.hdr("x-backend-route-resp"), "backend-route-resp");
	assert_eq!(res.hdr("x-backend-resp"), "");
	let body = read_body(res.into_body()).await;
	assert_eq!(
		body.headers.get("x-route-req").unwrap().as_bytes(),
		b"route-req"
	);
	assert!(body.headers.get("x-backend-req").is_none(),);
	assert_eq!(
		body.headers.get("x-backend-route-req").unwrap().as_bytes(),
		b"backend-route-req"
	);
}
