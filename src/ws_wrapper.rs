use std::sync::{
	atomic::{AtomicBool, Ordering},
	Arc,
};

use event_listener::Event;
use flume::Receiver;
use futures_util::FutureExt;
use send_wrapper::SendWrapper;
use thiserror::Error;
use wisp_mux::{
	ws::{
		async_iterator_transport_read, async_iterator_transport_write, Payload, TransportRead,
		TransportWrite,
	},
	WispError,
};
use worker::{
	console_log,
	js_sys::{ArrayBuffer, Uint8Array},
	wasm_bindgen::{closure::Closure, JsCast},
	worker_sys::web_sys::{BinaryType, MessageEvent, WebSocket},
};

#[derive(Error, Debug)]
pub enum WebSocketError {
	#[error("Unknown JS WebSocket wrapper error: {0:?}")]
	Unknown(String),
	#[error("Failed to call WebSocket.send: {0:?}")]
	SendFailed(String),
	#[error("Failed to call WebSocket.close: {0:?}")]
	CloseFailed(String),
}

impl From<WebSocketError> for WispError {
	fn from(err: WebSocketError) -> Self {
		Self::WsImplError(Box::new(err))
	}
}

pub enum WebSocketMessage {
	Message(Vec<u8>),
}

pub struct WebSocketWrapper {
	pub inner: Arc<SendWrapper<WebSocket>>,
	close_event: Arc<Event>,
	closed: Arc<AtomicBool>,

	// used to retain the closures
	#[allow(dead_code)]
	onclose: SendWrapper<Closure<dyn Fn()>>,
	#[allow(dead_code)]
	onmessage: SendWrapper<Closure<dyn Fn(MessageEvent)>>,
}

pub struct WebSocketReader {
	read_rx: Receiver<WebSocketMessage>,
	closed: Arc<AtomicBool>,
	close_event: Arc<Event>,
}

impl WebSocketReader {
	pub fn into_read(self) -> impl TransportRead {
		Box::pin(async_iterator_transport_read(self, |this| {
			Box::pin(async {
				use WebSocketMessage as M;
				if this.closed.load(Ordering::Acquire) {
					return Err(WispError::WsImplSocketClosed);
				}

				let res = futures_util::select! {
					data = this.read_rx.recv_async() => data.ok(),
					() = this.close_event.listen().fuse() => None
				};

				match res {
					Some(M::Message(x)) => Ok(Some((Payload::from(x), this))),
					None => Ok(None),
				}
			})
		}))
	}
}

impl WebSocketWrapper {
	pub fn connect(ws: WebSocket) -> (Self, WebSocketReader) {
		let (read_tx, read_rx) = flume::unbounded();
		let closed = Arc::new(AtomicBool::new(false));

		let close_event = Arc::new(Event::new());

		let onmessage_tx = read_tx.clone();
		let onmessage = Closure::wrap(Box::new(move |evt: MessageEvent| {
			if let Ok(arr) = evt.data().dyn_into::<ArrayBuffer>() {
				let _ =
					onmessage_tx.send(WebSocketMessage::Message(Uint8Array::new(&arr).to_vec()));
			}
		}) as Box<dyn Fn(MessageEvent)>);

		let onclose_closed = closed.clone();
		let onclose_event = close_event.clone();
		let onclose = Closure::wrap(Box::new(move || {
			onclose_closed.store(true, Ordering::Release);
			onclose_event.notify(usize::MAX);
		}) as Box<dyn Fn()>);

		ws.set_binary_type(BinaryType::Arraybuffer);
		ws.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
		ws.set_onclose(Some(onclose.as_ref().unchecked_ref()));

		(
			Self {
				inner: Arc::new(SendWrapper::new(ws)),
				close_event: close_event.clone(),
				closed: closed.clone(),
				onclose: SendWrapper::new(onclose),
				onmessage: SendWrapper::new(onmessage),
			},
			WebSocketReader {
				read_rx,
				closed,
				close_event,
			},
		)
	}

	pub fn into_write(self) -> impl TransportWrite {
		let ws = self.inner.clone();
		let closed = self.closed.clone();
		let close_event = self.close_event.clone();
		Box::pin(async_iterator_transport_write(
			self,
			|this, item| {
				Box::pin(async move {
					this.inner
						.send_with_js_u8_array(&Uint8Array::from(&item[..]))
						.map_err(|x| WebSocketError::SendFailed(format!("{x:?}")))?;
					Ok(this)
				})
			},
			(ws, closed, close_event),
			|(ws, closed, close_event)| {
				Box::pin(async move {
					ws.set_onopen(None);
					ws.set_onclose(None);
					ws.set_onerror(None);
					ws.set_onmessage(None);
					closed.store(true, Ordering::Release);
					close_event.notify(usize::MAX);

					ws.close()
						.map_err(|x| WebSocketError::CloseFailed(format!("{x:?}")).into())
				})
			},
		))
	}
}
