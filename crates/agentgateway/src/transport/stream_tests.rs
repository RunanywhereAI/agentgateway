//! What a keepalive with a dead-peer bound does to a real TCP connection (Linux, loopback).
//!
//! A peer that vanishes without a FIN (a pod killed under the network, a lost node) sends nothing
//! the other side could notice. The kernel's keepalive probes end an idle connection, but send
//! nothing while data is unacknowledged, so a connection that is being written to ends only when
//! the kernel gives up retransmitting, about 15 minutes later. The user timeout ends it in the
//! keepalive's own time. A peer whose receive window is shut (`wedged_peer`) is how a test makes
//! the sender's data stay unsent without dropping packets, which needs privileges a test does not
//! have: the sender's kernel then probes the window until the user timeout, or forever without it.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use socket2::{Domain, Type};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::*;
use crate::BackendConfig;

fn seconds(n: u64) -> Duration {
	Duration::from_secs(n)
}

fn config(keepalives: KeepaliveConfig) -> BackendConfig {
	BackendConfig {
		keepalives,
		..Default::default()
	}
}

/// A keepalive that declares an idle peer dead in 2 seconds (1 s, then one probe 1 s later).
fn two_second_keepalive() -> KeepaliveConfig {
	KeepaliveConfig {
		enabled: true,
		time: seconds(1),
		interval: seconds(1),
		retries: 1,
		user_timeout: None,
	}
}

fn user_timeout_of(socket: &Socket) -> Option<Duration> {
	let SocketType::Tcp(tcp) = &socket.inner else {
		panic!("the socket is not a TCP socket");
	};
	socket2::SockRef::from(tcp)
		.tcp_user_timeout()
		.expect("the user timeout can be read")
}

/// A server that accepts one connection and never reads from it. Its receive buffer is the smallest
/// the kernel allows (the accepted socket inherits it from the listener), so after a few KB the
/// window it offers is shut: the sender's data stays unsent, and its kernel can only probe the
/// window, which the peer's kernel keeps answering.
async fn wedged_peer() -> (SocketAddr, tokio::task::JoinHandle<()>) {
	let socket = socket2::Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
	socket.set_recv_buffer_size(1).unwrap();
	socket
		.bind(&SocketAddr::from(([127, 0, 0, 1], 0)).into())
		.unwrap();
	socket.listen(8).unwrap();
	socket.set_nonblocking(true).unwrap();
	let listener = TcpListener::from_std(socket.into()).unwrap();
	let address = listener.local_addr().unwrap();
	let task = tokio::spawn(async move {
		let (connection, _) = listener.accept().await.unwrap();
		std::future::pending::<()>().await;
		drop(connection);
	});
	(address, task)
}

/// Writes until the connection fails or `within` passes. The result is the error, or None when the
/// connection was still up at `within`.
async fn write_until_it_fails(socket: &mut Socket, within: Duration) -> Option<io::Error> {
	let chunk = vec![7u8; 16 * 1024];
	let writing = async {
		loop {
			if let Err(error) = socket.write_all(&chunk).await {
				return error;
			}
		}
	};
	tokio::time::timeout(within, writing).await.ok()
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_policy_keepalive_gets_the_dead_peer_bound() {
	let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
	let keepalives = KeepaliveConfig {
		enabled: true,
		time: seconds(5),
		interval: seconds(3),
		retries: 3,
		user_timeout: None,
	}
	.with_dead_peer_bound();
	assert_eq!(keepalives.user_timeout, Some(seconds(14)));
	let socket = Socket::dial(listener.local_addr().unwrap(), &config(keepalives))
		.await
		.unwrap();
	assert_eq!(user_timeout_of(&socket), Some(seconds(14)));
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn the_default_keepalive_and_a_disabled_one_set_no_user_timeout() {
	let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
	let address = listener.local_addr().unwrap();
	let socket = Socket::dial(address, &BackendConfig::default())
		.await
		.unwrap();
	assert_eq!(user_timeout_of(&socket), None);
	let disabled = KeepaliveConfig {
		enabled: false,
		..two_second_keepalive().with_dead_peer_bound()
	};
	let socket = Socket::dial(address, &config(disabled)).await.unwrap();
	assert_eq!(user_timeout_of(&socket), None);
}

#[test]
fn the_bound_is_what_the_probes_take_and_the_dead_peer_keepalive_is_fourteen_seconds() {
	assert_eq!(two_second_keepalive().give_up_after(), seconds(2));
	assert_eq!(KeepaliveConfig::dead_peer().give_up_after(), seconds(14));
	assert_eq!(KeepaliveConfig::dead_peer().user_timeout, Some(seconds(14)));
	assert_eq!(KeepaliveConfig::default().user_timeout, None);
	let huge = KeepaliveConfig {
		enabled: true,
		time: Duration::MAX,
		interval: Duration::MAX,
		retries: u32::MAX,
		user_timeout: None,
	};
	assert_eq!(huge.give_up_after(), Duration::MAX);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_peer_that_goes_silent_with_data_in_flight_ends_the_connection_at_the_bound() {
	let (address, _peer) = wedged_peer().await;
	let mut socket = Socket::dial(
		address,
		&config(two_second_keepalive().with_dead_peer_bound()),
	)
	.await
	.unwrap();
	let started = Instant::now();
	let error = write_until_it_fails(&mut socket, seconds(60))
		.await
		.expect("the connection to a peer that stopped acknowledging must end");
	assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{error:?}");
	// The bound is 2 seconds from the first window probe; a loaded test machine may be slower, a
	// kernel that gave up retransmitting on its own would take minutes.
	assert!(
		started.elapsed() < seconds(30),
		"it took {:?}",
		started.elapsed()
	);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn without_the_bound_a_peer_that_goes_silent_keeps_the_connection_open() {
	let (address, _peer) = wedged_peer().await;
	let mut socket = Socket::dial(address, &config(two_second_keepalive()))
		.await
		.unwrap();
	// Three times the time the bound takes.
	let error = write_until_it_fails(&mut socket, seconds(8)).await;
	assert!(error.is_none(), "the connection ended without a bound: {error:?}");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_peer_that_acknowledges_but_sends_nothing_is_not_cut_at_the_bound() {
	let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
	let address = listener.local_addr().unwrap();
	let server = tokio::spawn(async move {
		let (mut connection, _) = listener.accept().await.unwrap();
		let mut buffer = [0u8; 4];
		connection.read_exact(&mut buffer).await.unwrap();
		connection.write_all(&buffer).await.unwrap();
	});
	let mut socket = Socket::dial(
		address,
		&config(two_second_keepalive().with_dead_peer_bound()),
	)
	.await
	.unwrap();
	// Idle for three times the bound: the peer's kernel answers the probes, so the connection is
	// alive, and a stream that is only waiting (a queue, a slow first token) is not cut.
	tokio::time::sleep(seconds(6)).await;
	socket.write_all(b"ping").await.unwrap();
	let mut echoed = [0u8; 4];
	socket.read_exact(&mut echoed).await.unwrap();
	assert_eq!(&echoed, b"ping");
	server.await.unwrap();
}
