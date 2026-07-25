// risc-box patch: pluggable network backend for the virtio-net device,
// mirroring the shape of `terminal::Terminal` (guest-side methods called by
// the device, host-side methods called by the embedding application).

/// Transfers ethernet frames between the emulated virtio-net device and
/// whatever network the embedding application provides. Frames are whole
/// ethernet-II frames without FCS (dst mac | src mac | ethertype | payload).
pub trait NetBackend {
	/// Guest transmitted a frame (called by the virtio-net device).
	fn guest_tx(&mut self, frame: Vec<u8>);

	/// Next frame to deliver into the guest, if any (called by the
	/// virtio-net device when the driver has receive buffers posted).
	fn guest_rx(&mut self) -> Option<Vec<u8>>;

	/// Host queues a frame for delivery into the guest.
	fn host_push(&mut self, frame: Vec<u8>);

	/// Host takes the next guest-transmitted frame, if any.
	fn host_pop(&mut self) -> Option<Vec<u8>>;
}

/// Backend used when the application never attaches a network: transmitted
/// frames are dropped on the floor and nothing is ever received. The guest
/// sees a cable with no link partner.
pub struct NullNetBackend {}

impl NullNetBackend {
	pub fn new() -> Self {
		NullNetBackend {}
	}
}

impl NetBackend for NullNetBackend {
	fn guest_tx(&mut self, _frame: Vec<u8>) {}
	fn guest_rx(&mut self) -> Option<Vec<u8>> { None }
	fn host_push(&mut self, _frame: Vec<u8>) {}
	fn host_pop(&mut self) -> Option<Vec<u8>> { None }
}
