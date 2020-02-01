//! A socket handling library for those running in Tokio environments who wish to use
//! rust-lightning with native TcpStreams.
//!
//! Designed to be as simple as possible, the high-level usage is almost as simple as "hand over a
//! TcpStream and a reference to a PeerManager and the rest is handled", except for the
//! [Event](../lightning/util/events/enum.Event.html) handlng mechanism, see below.
//!
//! The PeerHandler, due to the fire-and-forget nature of this logic, must be an Arc, and must use
//! the SocketDescriptor provided here as the PeerHandler's SocketDescriptor.
//!
//! Three methods are exposed to register a new connection for handling in tokio::spawn calls, see
//! their individual docs for more. All three take a
//! [mpsc::Sender<()>](../tokio/sync/mpsc/struct.Sender.html) which is sent into every time
//! something occurs which may result in lightning [Events](../lightning/util/events/enum.Event.html).
//! The call site should, thus, look something like this:
//! ```
//! use tokio::sync::mpsc;
//! use tokio::net::TcpStream;
//! use secp256k1::key::PublicKey;
//! use lightning::util::events::EventsProvider;
//! use std::net::SocketAddr;
//! use std::sync::Arc;
//!
//! // Define concrete types for our high-level objects:
//! type TxBroadcaster = Arc<dyn lightning::chain::chaininterface::BroadcasterInterface>;
//! type ChannelMonitor = lightning::ln::channelmonitor::SimpleManyChannelMonitor<lightning::chain::transaction::OutPoint, lightning::chain::keysinterface::InMemoryChannelKeys, TxBroadcaster>;
//! type ChannelManager = lightning::ln::channelmanager::SimpleArcChannelManager<ChannelMonitor, dyn lightning::chain::chaininterface::BroadcasterInterface>;
//! type PeerManager = lightning::ln::peer_handler::SimpleArcPeerManager<lightning_net_tokio::SocketDescriptor, ChannelMonitor, dyn lightning::chain::chaininterface::BroadcasterInterface>;
//!
//! // Connect to node with pubkey their_node_id at addr:
//! async fn connect_to_node(peer_manager: PeerManager, channel_monitor: Arc<ChannelMonitor>, channel_manager: ChannelManager, their_node_id: PublicKey, addr: SocketAddr) {
//!     let (sender, mut receiver) = mpsc::channel(2);
//!     lightning_net_tokio::connect_outbound(peer_manager, sender, their_node_id, addr).await;
//!     loop {
//!         receiver.recv().await;
//!         for _event in channel_manager.get_and_clear_pending_events().drain(..) {
//!             // Handle the event!
//!         }
//!         for _event in channel_monitor.get_and_clear_pending_events().drain(..) {
//!             // Handle the event!
//!         }
//!     }
//! }
//!
//! // Begin reading from a newly accepted socket and talk to the peer:
//! async fn accept_socket(peer_manager: PeerManager, channel_monitor: Arc<ChannelMonitor>, channel_manager: ChannelManager, socket: TcpStream) {
//!     let (sender, mut receiver) = mpsc::channel(2);
//!     lightning_net_tokio::setup_inbound(peer_manager, sender, socket);
//!     loop {
//!         receiver.recv().await;
//!         for _event in channel_manager.get_and_clear_pending_events().drain(..) {
//!             // Handle the event!
//!         }
//!         for _event in channel_monitor.get_and_clear_pending_events().drain(..) {
//!             // Handle the event!
//!         }
//!     }
//! }

//! ```

use secp256k1::key::PublicKey;

use tokio::net::TcpStream;
use tokio::{io, time};
use tokio::sync::{mpsc, oneshot};
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};

use lightning::ln::peer_handler;
use lightning::ln::peer_handler::SocketDescriptor as LnSocketTrait;
use lightning::ln::msgs::ChannelMessageHandler;

use std::task;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use std::hash::Hash;

static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Connection contains all our internal state for a connection - we hold a reference to the
/// Connection object (in an Arc<Mutex<>>) in each SocketDescriptor we create as well as in the
/// read future (which is returned by schedule_read).
struct Connection {
	writer: Option<io::WriteHalf<TcpStream>>,
	event_notify: mpsc::Sender<()>,
	// Because our PeerManager is templated by user-provided types, and we can't (as far as I can
	// tell) have a const RawWakerVTable built out of templated functions, we need some indirection
	// between being woken up with write-ready and calling PeerManager::write_buffer_spce_avail.
	// This provides that indirection, with a Sender which gets handed to the PeerManager Arc on
	// the schedule_read stack.
	//
	// An alternative (likely more effecient) approach would involve creating a RawWakerVTable at
	// runtime with functions templated by the Arc<PeerManager> type, calling
	// write_buffer_space_avail directly from tokio's write wake, however doing so would require
	// more unsafe voodo than I really feel like writing.
	write_avail: mpsc::Sender<()>,
	// When we are told by rust-lightning to pause read (because we have writes backing up), we do
	// so by setting read_paused. If the read thread thereafter reads some data, it will place a
	// Sender here and then block on it. When we get a send_data call with resume_read set, we will
	// send into any read_blocker to wake the reading future back up and set read_paused back to
	// false.
	read_blocker: Option<oneshot::Sender<()>>,
	read_paused: bool,
	// If we get disconnected via SocketDescriptor::disconnect_socket(), we don't call
	// disconnect_event(), but if we get an Err return value out of PeerManager, in general, we do.
	// We track here whether we'll need to call disconnect_event() after the socket closes.
	need_disconnect_event: bool,
	disconnect: bool,
	id: u64,
}
impl Connection {
	async fn schedule_read<CMH: ChannelMessageHandler + 'static>(peer_manager: Arc<peer_handler::PeerManager<SocketDescriptor, Arc<CMH>>>, us: Arc<Mutex<Self>>, mut reader: io::ReadHalf<TcpStream>, mut write_avail_receiver: mpsc::Receiver<()>) {
		let peer_manager_ref = peer_manager.clone();
		// 8KB is nice and big but also should never cause any issues with stack overflowing.
		let mut buf = [0; 8192];
		loop {
			macro_rules! shutdown_socket {
				($err: expr) => { {
					println!("Disconnecting peer due to {}!", $err);
					break;
				} }
			}

			// Whenever we want to block on reading or waiting for reading to resume, we have to
			// at least select with the write_avail_receiver, which is used by the
			// SocketDescriptor to wake us up if we need to shut down the socket or if we need
			// to generate a write_buffer_space_avail call.
			macro_rules! select_write_ev {
				($v: expr) => { {
					assert!($v.is_some()); // We can't have dropped the sending end, its in the us Arc!
					if us.lock().unwrap().disconnect {
						shutdown_socket!("disconnect_socket() call from RL");
					}
					if let Err(e) = peer_manager.write_buffer_space_avail(&mut SocketDescriptor::new(us.clone())) {
						us.lock().unwrap().need_disconnect_event = false;
						shutdown_socket!(e);
					}
				} }
			}

			tokio::select! {
				v = write_avail_receiver.recv() => select_write_ev!(v),
				read = reader.read(&mut buf) => match read {
					Ok(0) => {
						println!("Connection closed");
						break;
					},
					Ok(len) => {
						if let Some(blocker) = {
							let mut lock = us.lock().unwrap();
							if lock.disconnect {
								shutdown_socket!("disconnect_socket() call from RL");
							}
							if lock.read_paused {
								let (sender, blocker) = oneshot::channel();
								lock.read_blocker = Some(sender);
								Some(blocker)
							} else { None }
						} {
							tokio::select! {
								res = blocker => {
									res.unwrap(); // We should never drop the sender without sending () into it!
									if us.lock().unwrap().disconnect {
										shutdown_socket!("disconnect_socket() call from RL");
									}
								},
								v = write_avail_receiver.recv() => select_write_ev!(v),
							}
						}
						match peer_manager.read_event(&mut SocketDescriptor::new(Arc::clone(&us)), &buf[0..len]) {
							Ok(pause_read) => {
								if pause_read {
									let mut lock = us.lock().unwrap();
									lock.read_paused = true;
								}

								match us.lock().unwrap().event_notify.try_send(()) {
									Ok(_) => {},
									Err(mpsc::error::TrySendError::Full(_)) => {
										// Ignore full errors as we just need the user to poll after this point, so if they
										// haven't received the last send yet, it doesn't matter.
									},
									_ => panic!()
								}
							},
							Err(e) => {
								us.lock().unwrap().need_disconnect_event = false;
								shutdown_socket!(e)
							},
						}
					},
					Err(e) => {
						println!("Connection closed: {}", e);
						break;
					},
				},
			}
		}
		let writer_option = us.lock().unwrap().writer.take();
		if let Some(mut writer) = writer_option {
			// If the socket is already closed, shutdown() will fail, so just ignore it.
			let _ = writer.shutdown().await;
		}
		if us.lock().unwrap().need_disconnect_event {
			peer_manager_ref.socket_disconnected(&SocketDescriptor::new(Arc::clone(&us)));
			match us.lock().unwrap().event_notify.try_send(()) {
				Ok(_) => {},
				Err(mpsc::error::TrySendError::Full(_)) => {
					// Ignore full errors as we just need the user to poll after this point, so if they
					// haven't received the last send yet, it doesn't matter.
				},
				_ => panic!()
			}
		}
	}

	fn new(event_notify: mpsc::Sender<()>, stream: TcpStream) -> (io::ReadHalf<TcpStream>, mpsc::Receiver<()>, Arc<Mutex<Self>>) {
		// We only ever need a channel of depth 1 here: if we returned a non-full write to the
		// PeerManager, we will eventually get notified that there is room in the socket to write
		// new bytes, which will generate an event. That event will be popped off the queue before
		// we call write_buffer_space_avail, ensuring that we have room to push a new () if, during
		// the write_buffer_space_avail() call, send_data() returns a non-full write.
		let (write_avail, receiver) = mpsc::channel(1);
		let (reader, writer) = io::split(stream);

		(reader, receiver,
		Arc::new(Mutex::new(Self {
			writer: Some(writer), event_notify, write_avail,
			read_blocker: None, read_paused: false, need_disconnect_event: true, disconnect: false,
			id: ID_COUNTER.fetch_add(1, Ordering::AcqRel)
		})))
	}
}

/// Process incoming messages and feed outgoing messages on the provided socket generated by
/// accepting an incoming connection.
///
/// The returned future will complete when the peer is disconnected and associated handling
/// futures are freed, though, because all processing futures are spawned with tokio::spawn, you do
/// not need to poll the provided future in order to make progress.
///
/// See the module-level documentation for how to handle the event_notify mpsc::Sender.
pub fn setup_inbound<CMH: ChannelMessageHandler + 'static>(peer_manager: Arc<peer_handler::PeerManager<SocketDescriptor, Arc<CMH>>>, event_notify: mpsc::Sender<()>, stream: TcpStream) -> impl std::future::Future<Output=()> {
	let (reader, receiver, us) = Connection::new(event_notify, stream);

	let handle_opt = if let Ok(_) = peer_manager.new_inbound_connection(SocketDescriptor::new(us.clone())) {
		Some(tokio::spawn(Connection::schedule_read(peer_manager, us, reader, receiver)))
	} else {
		// Note that we will skip disconnect_event here, in accordance with the PeerManager
		// requirements, as disconnect_event is called by the schedule_read Future.
		None
	};

	async move {
		if let Some(handle) = handle_opt {
			if let Err(e) = handle.await {
				assert!(e.is_cancelled());
			}
		}
	}
}

/// Process incoming messages and feed outgoing messages on the provided socket generated by
/// making an outbound connection which is expected to be accepted by a peer with the given
/// public key. The relevant processing is set to run free (via tokio::spawn).
///
/// The returned future will complete when the peer is disconnected and associated handling
/// futures are freed, though, because all processing futures are spawned with tokio::spawn, you do
/// not need to poll the provided future in order to make progress.
///
/// See the module-level documentation for how to handle the event_notify mpsc::Sender.
pub fn setup_outbound<CMH: ChannelMessageHandler + 'static>(peer_manager: Arc<peer_handler::PeerManager<SocketDescriptor, Arc<CMH>>>, event_notify: mpsc::Sender<()>, their_node_id: PublicKey, stream: TcpStream) -> impl std::future::Future<Output=()> {
	let (reader, receiver, us) = Connection::new(event_notify, stream);

	let handle_opt = if let Ok(initial_send) = peer_manager.new_outbound_connection(their_node_id, SocketDescriptor::new(us.clone())) {
		Some(tokio::spawn(async move {
			if SocketDescriptor::new(us.clone()).send_data(&initial_send, true) != initial_send.len() {
				// We should essentially always have enough room in a TCP socket buffer to send the
				// initial 10s of bytes, if not, just give up as hopeless.
				eprintln!("Failed to write first full message to socket!");
				peer_manager.socket_disconnected(&SocketDescriptor::new(Arc::clone(&us)));
			} else {
				Connection::schedule_read(peer_manager, us, reader, receiver).await;
			}
		}))
	} else {
		// Note that we will skip disconnect_event here, in accordance with the PeerManager
		// requirements, as disconnect_event is called by the schedule_read Future.
		None
	};

	async move {
		if let Some(handle) = handle_opt {
			if let Err(e) = handle.await {
				assert!(e.is_cancelled());
			}
		}
	}
}

/// Process incoming messages and feed outgoing messages on a new connection made to the given
/// socket address which is expected to be accepted by a peer with the given public key (by
/// scheduling futures with tokio::spawn).
///
/// Shorthand for TcpStream::connect(addr) with a timeout followed by setup_outbound().
///
/// Returns a future (as the fn is async) which needs to be polled to complete the connection and
/// connection setup. That future then returns a future which will complete when the peer is
/// disconnected and associated handling futures are freed, though, because all processing in said
/// futures are spawned with tokio::spawn, you do not need to poll the second future in order to
/// make progress.
///
/// See the module-level documentation for how to handle the event_notify mpsc::Sender.
pub async fn connect_outbound<CMH: ChannelMessageHandler + 'static>(peer_manager: Arc<peer_handler::PeerManager<SocketDescriptor, Arc<CMH>>>, event_notify: mpsc::Sender<()>, their_node_id: PublicKey, addr: SocketAddr) -> Option<impl std::future::Future<Output=()>> {
	let connect_timeout = time::delay_for(Duration::from_secs(10));
	let connect_fut = TcpStream::connect(&addr);
	tokio::select! {
		_ = connect_timeout => None,
		res = connect_fut => {
			if let Ok(stream) = res {
				Some(setup_outbound(peer_manager, event_notify, their_node_id, stream))
			} else { None }
		},
	}
}

const SOCK_WAKER_VTABLE: task::RawWakerVTable =
	task::RawWakerVTable::new(clone_socket_waker, wake_socket_waker, wake_socket_waker_by_ref, drop_socket_waker);

fn clone_socket_waker(orig_ptr: *const ()) -> task::RawWaker {
	descriptor_to_waker(orig_ptr as *const SocketDescriptor)
}
fn wake_socket_waker(orig_ptr: *const ()) {
	wake_socket_waker_by_ref(orig_ptr);
	drop_socket_waker(orig_ptr);
}
fn wake_socket_waker_by_ref(orig_ptr: *const ()) {
	let descriptor = orig_ptr as *const SocketDescriptor;
	// An error should be fine. Most likely we got two send_datas in a row, both of which failed to
	// fully write, but we only need to call write_buffer_space_avail() once. Otherwise, the
	// sending thread may have already gone away due to a socket close, in which case there's
	// nothing to wake up anyway.
	let mut us = unsafe { (*descriptor).conn.lock() }.unwrap();
	let _ = us.write_avail.try_send(());
}
fn drop_socket_waker(orig_ptr: *const ()) {
	let _orig_box = unsafe { Box::from_raw(orig_ptr as *mut SocketDescriptor) };
	// _orig_box is now dropped
}
fn descriptor_to_waker(descriptor: *const SocketDescriptor) -> task::RawWaker {
	let new_box = Box::leak(Box::new(unsafe { (*descriptor).clone() }));
	let new_ptr = new_box as *const SocketDescriptor;
	task::RawWaker::new(new_ptr as *const (), &SOCK_WAKER_VTABLE)
}

/// The SocketDescriptor used to refer to sockets by a PeerHandler. This is pub only as it is a
/// type in the template of PeerHandler.
pub struct SocketDescriptor {
	conn: Arc<Mutex<Connection>>,
	id: u64,
}
impl SocketDescriptor {
	fn new(conn: Arc<Mutex<Connection>>) -> Self {
		let id = conn.lock().unwrap().id;
		Self { conn, id }
	}
}
impl peer_handler::SocketDescriptor for SocketDescriptor {
	fn send_data(&mut self, data: &[u8], resume_read: bool) -> usize {
		// To send data, we take a lock on our Connection to access the WriteHalf of the TcpStream,
		// writing to it if there's room in the kernel buffer, or otherwise create a new Waker with
		// a SocketDescriptor in it which can wake up the write_avail Sender, waking up the
		// processing future which will call write_buffer_space_avail and we'll end up back here.
		let mut us = self.conn.lock().unwrap();
		if us.writer.is_none() {
			// The writer gets take()n when it is time to shut down, so just fast-return 0 here.
			return 0;
		}

		if resume_read {
			if let Some(sender) = us.read_blocker.take() {
				// The schedule_read future may go to lock up but end up getting woken up by there
				// being more room in the write buffer, dropping the other end of this Sender
				// before we get here, so we ignore any failures to wake it up.
				let _ = sender.send(());
			}
			us.read_paused = false;
		}
		if data.is_empty() { return 0; }
		let waker = unsafe { task::Waker::from_raw(descriptor_to_waker(self)) };
		let mut ctx = task::Context::from_waker(&waker);
		let mut written_len = 0;
		loop {
			match std::pin::Pin::new(us.writer.as_mut().unwrap()).poll_write(&mut ctx, &data[written_len..]) {
				task::Poll::Ready(Ok(res)) => {
					// The tokio docs *seem* to indicate this can't happen, and I certainly don't
					// know how to handle it if it does (cause it should be a Poll::Pending
					// instead):
					assert_ne!(res, 0);
					written_len += res;
					if written_len == data.len() { return written_len; }
				},
				task::Poll::Ready(Err(e)) => {
					// The tokio docs *seem* to indicate this can't happen, and I certainly don't
					// know how to handle it if it does (cause it should be a Poll::Pending
					// instead):
					assert_ne!(e.kind(), io::ErrorKind::WouldBlock);
					// Probably we've already been closed, just return what we have and let the
					// read thread handle closing logic.
					return written_len;
				},
				task::Poll::Pending => {
					// We're queued up for a write event now, but we need to make sure we also
					// pause read given we're now waiting on the remote end to ACK (and in
					// accordance with the send_data() docs).
					us.read_paused = true;
					return written_len;
				},
			}
		}
	}

	fn disconnect_socket(&mut self) {
		let mut us = self.conn.lock().unwrap();
		us.need_disconnect_event = false;
		us.disconnect = true;
		us.read_paused = true;
		// Wake up the sending thread, assuming it is still alive
		let _ = us.write_avail.try_send(());
		// TODO: There's a race where we don't meet the requirements of disconnect_socket if the
		// read task is about to call a PeerManager function (eg read_event or write_event).
		// Ideally we need to release the us lock and block until we have confirmation from the
		// read task that it has broken out of its main loop.
	}
}
impl Clone for SocketDescriptor {
	fn clone(&self) -> Self {
		Self {
			conn: Arc::clone(&self.conn),
			id: self.id,
		}
	}
}
impl Eq for SocketDescriptor {}
impl PartialEq for SocketDescriptor {
	fn eq(&self, o: &Self) -> bool {
		self.id == o.id
	}
}
impl Hash for SocketDescriptor {
	fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
		self.id.hash(state);
	}
}

#[cfg(test)]
mod tests {
	use lightning::ln::features::*;
	use lightning::ln::msgs::*;
	use lightning::ln::peer_handler::{MessageHandler, PeerManager};
	use lightning::util::events::*;
	use secp256k1::{Secp256k1, SecretKey, PublicKey};

	use tokio::sync::mpsc;

	use std::sync::{Arc, Mutex};
	use std::time::Duration;

	pub struct TestLogger();
	impl lightning::util::logger::Logger for TestLogger {
		fn log(&self, record: &lightning::util::logger::Record) {
			println!("{:<5} [{} : {}, {}] {}", record.level.to_string(), record.module_path, record.file, record.line, record.args);
		}
	}

	struct MsgHandler{
		disconnect_keys: Mutex<Vec<PublicKey>>,
		connected_peers: Mutex<Vec<PublicKey>>,
	}
	impl RoutingMessageHandler for MsgHandler {
		fn handle_node_announcement(&self, _msg: &NodeAnnouncement) -> Result<bool, LightningError> { Ok(false) }
		fn handle_channel_announcement(&self, _msg: &ChannelAnnouncement) -> Result<bool, LightningError> { Ok(false) }
		fn handle_channel_update(&self, _msg: &ChannelUpdate) -> Result<bool, LightningError> { Ok(false) }
		fn handle_htlc_fail_channel_update(&self, _update: &HTLCFailChannelUpdate) { }
		fn get_next_channel_announcements(&self, _starting_point: u64, _batch_amount: u8) -> Vec<(ChannelAnnouncement, ChannelUpdate, ChannelUpdate)> { Vec::new() }
		fn get_next_node_announcements(&self, _starting_point: Option<&PublicKey>, _batch_amount: u8) -> Vec<NodeAnnouncement> { Vec::new() }
		fn should_request_full_sync(&self, _node_id: &PublicKey) -> bool { false }
	}
	impl ChannelMessageHandler for MsgHandler {
		fn handle_open_channel(&self, _their_node_id: &PublicKey, _their_features: InitFeatures, _msg: &OpenChannel) {}
		fn handle_accept_channel(&self, _their_node_id: &PublicKey, _their_features: InitFeatures, _msg: &AcceptChannel) {}
		fn handle_funding_created(&self, _their_node_id: &PublicKey, _msg: &FundingCreated) {}
		fn handle_funding_signed(&self, _their_node_id: &PublicKey, _msg: &FundingSigned) {}
		fn handle_funding_locked(&self, _their_node_id: &PublicKey, _msg: &FundingLocked) {}
		fn handle_shutdown(&self, _their_node_id: &PublicKey, _msg: &Shutdown) {}
		fn handle_closing_signed(&self, _their_node_id: &PublicKey, _msg: &ClosingSigned) {}
		fn handle_update_add_htlc(&self, _their_node_id: &PublicKey, _msg: &UpdateAddHTLC) {}
		fn handle_update_fulfill_htlc(&self, _their_node_id: &PublicKey, _msg: &UpdateFulfillHTLC) {}
		fn handle_update_fail_htlc(&self, _their_node_id: &PublicKey, _msg: &UpdateFailHTLC) {}
		fn handle_update_fail_malformed_htlc(&self, _their_node_id: &PublicKey, _msg: &UpdateFailMalformedHTLC) {}
		fn handle_commitment_signed(&self, _their_node_id: &PublicKey, _msg: &CommitmentSigned) {}
		fn handle_revoke_and_ack(&self, _their_node_id: &PublicKey, _msg: &RevokeAndACK) {}
		fn handle_update_fee(&self, _their_node_id: &PublicKey, _msg: &UpdateFee) {}
		fn handle_announcement_signatures(&self, _their_node_id: &PublicKey, _msg: &AnnouncementSignatures) {}
		fn peer_disconnected(&self, their_node_id: &PublicKey, _no_connection_possible: bool) {
			self.connected_peers.lock().unwrap().retain(|key| key != their_node_id);
		}
		fn peer_connected(&self, their_node_id: &PublicKey, _msg: &Init) {
			self.connected_peers.lock().unwrap().push(their_node_id.clone());
		}
		fn handle_channel_reestablish(&self, _their_node_id: &PublicKey, _msg: &ChannelReestablish) {}
		fn handle_error(&self, _their_node_id: &PublicKey, _msg: &ErrorMessage) {}
	}
	impl MessageSendEventsProvider for MsgHandler {
		fn get_and_clear_pending_msg_events(&self) -> Vec<MessageSendEvent> {
			self.disconnect_keys.lock().unwrap().drain(..).map(|node_id| MessageSendEvent::HandleError {
				node_id, action: ErrorAction::DisconnectPeer { msg: None }
			}).collect()
		}
	}

	#[tokio::test(threaded_scheduler)]
	async fn basic_connection_test() {
		let a_handler = Arc::new(MsgHandler {
			disconnect_keys: Mutex::new(Vec::new()),
			connected_peers: Mutex::new(Vec::new()),
		});
		let a_key = SecretKey::from_slice(&[1; 32]).unwrap();
		let a_manager = Arc::new(PeerManager::new(MessageHandler {
			chan_handler: Arc::clone(&a_handler),
			route_handler: Arc::clone(&a_handler) as Arc<dyn RoutingMessageHandler>,
		}, a_key.clone(), &[1; 32], Arc::new(TestLogger())));

		let b_key = SecretKey::from_slice(&[1; 32]).unwrap();
		let b_handler = Arc::new(MsgHandler {
			disconnect_keys: Mutex::new(Vec::new()),
			connected_peers: Mutex::new(Vec::new()),
		});
		let b_manager = Arc::new(PeerManager::new(MessageHandler {
			chan_handler: Arc::clone(&b_handler),
			route_handler: Arc::clone(&b_handler) as Arc<dyn RoutingMessageHandler>,
		}, b_key.clone(), &[2; 32], Arc::new(TestLogger())));

		// We bind on localhost, hoping the environment is properly configured with a local
		// address. This may not always be the case in containers and the like, so if this test is
		// failing for you check that you have a loopback interface and it is configured with
		// 127.0.0.1.
		let (conn_a, conn_b) = if let Ok(listener) = std::net::TcpListener::bind("127.0.0.1:9735") {
			(std::net::TcpStream::connect("127.0.0.1:9735").unwrap(), listener.accept().unwrap().0)
		} else if let Ok(listener) = std::net::TcpListener::bind("127.0.0.1:9999") {
			(std::net::TcpStream::connect("127.0.0.1:9999").unwrap(), listener.accept().unwrap().0)
		} else if let Ok(listener) = std::net::TcpListener::bind("127.0.0.1:46926") {
			(std::net::TcpStream::connect("127.0.0.1:46926").unwrap(), listener.accept().unwrap().0)
		} else { panic!("Failed to bind to v4 localhost on common ports"); };

		let secp_ctx = Secp256k1::new();
		let a_pub = PublicKey::from_secret_key(&secp_ctx, &a_key);
		let b_pub = PublicKey::from_secret_key(&secp_ctx, &b_key);
		let (sender, _receiver) = mpsc::channel(2);
		let fut_a = super::setup_outbound(Arc::clone(&a_manager), sender.clone(), b_pub, tokio::net::TcpStream::from_std(conn_a).unwrap());
		let fut_b = super::setup_inbound(b_manager, sender, tokio::net::TcpStream::from_std(conn_b).unwrap());

		tokio::time::delay_for(Duration::from_secs(1)).await;
		assert!(b_handler.connected_peers.lock().unwrap().contains(&a_pub));
		assert!(a_handler.connected_peers.lock().unwrap().contains(&b_pub));

		a_handler.disconnect_keys.lock().unwrap().push(b_pub);
		a_manager.process_events();
		tokio::time::delay_for(Duration::from_secs(1)).await;
		assert!(!b_handler.connected_peers.lock().unwrap().contains(&a_pub));
		assert!(!a_handler.connected_peers.lock().unwrap().contains(&b_pub));

		fut_a.await;
		fut_b.await;
	}
}
