//! Basic endpoint implementations.
//! 
//! These should be used as basis or examples for more advanced endpoints provided by the library
//! or other crates.
use std::collections::{HashMap, VecDeque};
use std::io::{Error as IoError, ErrorKind as IoErrorKind};
use std::iter::repeat;
use std::net::{SocketAddr, UdpSocket};
use std::sync::Mutex;

use crate::connection::ConnectionId;
use crate::packet;

use super::{Transmit, TransmitError, Listen, Open};

/// Basic implementation of a client-side [`Endpoint`](Transmit).
/// 
/// Most trivial version of an endpoint implementation. Can be used as a basis for more
/// advanced endpoint versions.
#[derive(Debug)]
pub struct ClientEndpoint {
	socket: UdpSocket,
}

impl Transmit for ClientEndpoint {
	// Somewhat conservative 1200 byte estimate of MTU.
	const PACKET_BYTE_COUNT: usize = 1200;

	const RESERVED_BYTE_COUNT: usize = 0;

	#[inline]
	fn send_to(&self, data: &mut [u8], addr: SocketAddr) -> Result<usize, IoError> {
		debug_assert!(data.len() <= Self::PACKET_BYTE_COUNT);
		self.socket.send_to(data, addr)
	}

	fn recv_all(
		&self,
		buffer: &mut Vec<u8>,
		connection_id: ConnectionId,
	) -> Result<usize, TransmitError> {
		let mut recovered_bytes = 0;
		let mut work_offset = buffer.len();
		buffer.extend(repeat(0).take(Self::PACKET_BYTE_COUNT));
		loop {
			match self.socket.recv_from(&mut buffer[work_offset..]) {
				Ok((packet_size, _)) => {
					if packet_size == Self::PACKET_BYTE_COUNT
					&& packet::is_valid(&buffer[work_offset + Self::RESERVED_BYTE_COUNT ..])
					{
						recovered_bytes += packet_size;
						work_offset = buffer.len();
						buffer.extend(repeat(0).take(packet_size));
					}
					// otherwise the work_slice is reused
				},
				Err(error) => {
					// drop the work_slice
					buffer.truncate(buffer.len() - Self::PACKET_BYTE_COUNT);
					// NOTE the break!
					break match error.kind() {
						IoErrorKind::WouldBlock => {
							if recovered_bytes > 0 {
								Ok(recovered_bytes)
							} else {
								Err(TransmitError::NoPendingPackets)
							}
						}
						_ => Err(TransmitError::Io(error)),
					}
				}
			}
		}
	}
}

impl Open for ClientEndpoint {
	/// Open a new endpoint on provided local address with provided hasher.
	///
	/// The hasher will be used to validate packets.
	fn open(addr: SocketAddr) -> Result<Self, IoError> {
		let socket = UdpSocket::bind(addr)?;
		socket.set_nonblocking(true)?;
		Ok(Self {
			socket,
		})
	}
}

/// A UDP socket that caches packets for multiple connections that can be popped by
/// [`recv_all()`](Transmit::recv_all).
///
/// **NOTE**: that the [`Transmit`](Transmit) and [`Listen`](Listen) traits are only implemented
/// for [`Mutex`](Mutex)`<InternalServerEndpoint>`, as the server endpoint will not function correctly
/// otherwise.
#[derive(Debug)]
pub struct InternalServerEndpoint {
	socket: UdpSocket,
	connections: HashMap<ConnectionId, Vec<u8>>,
	packet_buffer: Box<[u8]>,
	connectionless_packets: VecDeque<(SocketAddr, Box<[u8]>)>,
}

impl InternalServerEndpoint {
	// Somewhat conservative 1200 byte estimate of MTU.
	const PACKET_BYTE_COUNT: usize = 1200;

	const RESERVED_BYTE_COUNT: usize = 0;

	fn recv_packets(&mut self) -> Result<(), IoError> {
		loop {
			match self.socket.recv_from(&mut self.packet_buffer) {
				Ok((packet_size, addr)) => {
					if packet_size == Self::PACKET_BYTE_COUNT
					&& packet::is_valid(&self.packet_buffer[Self::RESERVED_BYTE_COUNT ..])
					{
						let connection_id = packet::read_connection_id(
							&self.packet_buffer[Self::RESERVED_BYTE_COUNT ..],
						);
						if connection_id == 0 {
							self.connectionless_packets.push_back((addr, self.packet_buffer.clone()));
						} else if let Some(buffer) = self.connections.get_mut(&connection_id) {
							buffer.extend_from_slice(&self.packet_buffer);
						}
					}
				},
				Err(error) => match error.kind() {
					IoErrorKind::WouldBlock => break,
					_ => return Err(error),
				},
			}
		}
		Ok(())
	}

	/// Construct a new `InternalServerEndpoint` and bind it to provided local address.
	#[inline]
	fn open(addr: SocketAddr) -> Result<Self, IoError> {
		let socket = UdpSocket::bind(addr)?;
		socket.set_nonblocking(true)?;
		Ok(Self {
			socket,
			connections: HashMap::new(),
			packet_buffer: Box::new([0; Self::PACKET_BYTE_COUNT]),
			connectionless_packets: VecDeque::new(),
		})
	}
}

/// Alias for a functioning Endpoint struct. 
pub type ServerEndpoint = Mutex<InternalServerEndpoint>;

impl Transmit for ServerEndpoint {
	// Somewhat conservative 1200 byte estimate of MTU.
	const PACKET_BYTE_COUNT: usize = InternalServerEndpoint::PACKET_BYTE_COUNT;

	// 4 bytes reserved for the hash
	const RESERVED_BYTE_COUNT: usize = InternalServerEndpoint::RESERVED_BYTE_COUNT;

	#[inline]
	fn send_to(&self, data: &mut [u8], addr: SocketAddr) -> Result<usize, IoError> {
		let endpoint = self.lock().unwrap();
		endpoint.socket.send_to(data, addr)
	}

	fn recv_all(
		&self,
		buffer: &mut Vec<u8>,
		connection_id: ConnectionId,
	) -> Result<usize, TransmitError> {
		let mut endpoint = self.lock().unwrap();
		if let Err(error) = endpoint.recv_packets() {
			return Err(TransmitError::Io(error));
		};
		let reference_buffer = endpoint.connections.get_mut(&connection_id).unwrap();
		if reference_buffer.is_empty() {
			Err(TransmitError::NoPendingPackets)
		} else {
			buffer.extend(&reference_buffer[..]);
			let received_bytes = reference_buffer.len();
			reference_buffer.clear();
			Ok(received_bytes)
		}
	}
}

impl Listen for ServerEndpoint {
	fn allow_connection_id(&self, connection_id: ConnectionId) {
		let mut endpoint = self.lock().unwrap();
		endpoint.connections.insert(connection_id, Vec::new());
	}

	fn block_connection_id(&self, connection_id: ConnectionId) {
		let mut endpoint = self.lock().unwrap();
		endpoint.connections.remove(&connection_id);
	}

	fn pop_connectionless_packet(&self) -> Result<(SocketAddr, Box<[u8]>), TransmitError> {
		let mut endpoint = self.lock().unwrap();
		if let Err(error) = endpoint.recv_packets() {
			return Err(TransmitError::Io(error));
		};
		match endpoint.connectionless_packets.pop_front() {
			Some((addr, packet)) => Ok((addr, packet)),
			None => Err(TransmitError::NoPendingPackets),
		}
	}
}

impl Open for ServerEndpoint {
	fn open(addr: SocketAddr) -> Result<Self, IoError> {
		Ok(Mutex::new(InternalServerEndpoint::open(addr)?))
	}
}
