use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use async_trait::async_trait;
use bytes::BytesMut;
use event_listener::Event;
use flume::Receiver;
use futures_util::FutureExt;
use send_wrapper::SendWrapper;
use wisp_mux::{
    ws::{Frame, LockedWebSocketWrite, WebSocketRead, WebSocketWrite},
    WispError,
};
use worker::{
    js_sys::{ArrayBuffer, Uint8Array},
    wasm_bindgen::{closure::Closure, JsCast},
    worker_sys::{
        ext::WebSocketExt,
        web_sys::{BinaryType, MessageEvent, WebSocket},
    },
};

#[derive(Debug)]
pub enum WebSocketError {
    Unknown,
    SendFailed,
    CloseFailed,
}

impl std::fmt::Display for WebSocketError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        use WebSocketError::*;
        match self {
            Unknown => write!(f, "Unknown error"),
            SendFailed => write!(f, "Send failed"),
            CloseFailed => write!(f, "Close failed"),
        }
    }
}

impl std::error::Error for WebSocketError {}

impl From<WebSocketError> for WispError {
    fn from(err: WebSocketError) -> Self {
        Self::WsImplError(Box::new(err))
    }
}

#[derive(Debug)]
pub enum WebSocketMessage {
    Closed,
    Error,
    Message(Vec<u8>),
}

pub struct WebSocketWrapper {
    inner: SendWrapper<WebSocket>,
    open_event: Arc<Event>,
    error_event: Arc<Event>,
    close_event: Arc<Event>,
    closed: Arc<AtomicBool>,

    // used to retain the closures
    #[allow(dead_code)]
    onopen: SendWrapper<Closure<dyn Fn()>>,
    #[allow(dead_code)]
    onclose: SendWrapper<Closure<dyn Fn()>>,
    #[allow(dead_code)]
    onerror: SendWrapper<Closure<dyn Fn()>>,
    #[allow(dead_code)]
    onmessage: SendWrapper<Closure<dyn Fn(MessageEvent)>>,
}

pub struct WebSocketReader {
    read_rx: Receiver<WebSocketMessage>,
    closed: Arc<AtomicBool>,
    close_event: Arc<Event>,
}

#[async_trait]
impl WebSocketRead for WebSocketReader {
    async fn wisp_read_frame(&mut self, _: &LockedWebSocketWrite) -> Result<Frame, WispError> {
        use WebSocketMessage::*;
        if self.closed.load(Ordering::Acquire) {
            return Err(WispError::WsImplSocketClosed);
        }
        let res = futures_util::select! {
            data = self.read_rx.recv_async() => data.ok(),
            _ = self.close_event.listen().fuse() => Some(Closed),
        };
        match res.ok_or(WispError::WsImplSocketClosed)? {
            Message(bin) => Ok(Frame::binary(BytesMut::from(bin.as_slice()))),
            Error => Err(WebSocketError::Unknown.into()),
            Closed => Err(WispError::WsImplSocketClosed),
        }
    }
}

impl WebSocketWrapper {
    pub fn connect(ws: WebSocket) -> (Self, WebSocketReader) {
        let (read_tx, read_rx) = flume::unbounded();
        let closed = Arc::new(AtomicBool::new(false));

        let open_event = Arc::new(Event::new());
        let close_event = Arc::new(Event::new());
        let error_event = Arc::new(Event::new());

        let onopen_event = open_event.clone();
        let onopen = Closure::wrap(
            Box::new(move || while onopen_event.notify(usize::MAX) == 0 {}) as Box<dyn Fn()>,
        );

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

        let onerror_tx = read_tx.clone();
        let onerror_closed = closed.clone();
        let onerror_close = close_event.clone();
        let onerror_event = error_event.clone();
        let onerror = Closure::wrap(Box::new(move || {
            let _ = onerror_tx.send(WebSocketMessage::Error);
            onerror_closed.store(true, Ordering::Release);
            onerror_close.notify(usize::MAX);
            onerror_event.notify(usize::MAX);
        }) as Box<dyn Fn()>);

        ws.set_binary_type(BinaryType::Arraybuffer);
        ws.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
        ws.set_onopen(Some(onopen.as_ref().unchecked_ref()));
        ws.set_onclose(Some(onclose.as_ref().unchecked_ref()));
        ws.set_onerror(Some(onerror.as_ref().unchecked_ref()));

        ws.accept();

        (
            Self {
                inner: SendWrapper::new(ws),
                open_event,
                error_event,
                close_event: close_event.clone(),
                closed: closed.clone(),
                onopen: SendWrapper::new(onopen),
                onclose: SendWrapper::new(onclose),
                onerror: SendWrapper::new(onerror),
                onmessage: SendWrapper::new(onmessage),
            },
            WebSocketReader {
                read_rx,
                closed,
                close_event,
            },
        )
    }

    pub async fn wait_for_open(&self) -> bool {
        if self.closed.load(Ordering::Acquire) {
            return false;
        }
        futures_util::select! {
            _ = self.open_event.listen().fuse() => true,
            _ = self.error_event.listen().fuse() => false,
        }
    }
}

#[async_trait]
impl WebSocketWrite for WebSocketWrapper {
    async fn wisp_write_frame(&mut self, frame: Frame) -> Result<(), WispError> {
        use wisp_mux::ws::OpCode::*;
        if self.closed.load(Ordering::Acquire) {
            return Err(WispError::WsImplSocketClosed);
        }
        let arraybuffer = Uint8Array::from(frame.payload.as_ref());
        match frame.opcode {
            Binary | Text => self
                .inner
                .send_with_array_buffer(&arraybuffer.buffer())
                .map_err(|_| WebSocketError::SendFailed.into()),
            Close => {
                let _ = self.inner.close();
                Ok(())
            }
            _ => Err(WispError::WsImplNotSupported),
        }
    }

    async fn wisp_close(&mut self) -> Result<(), WispError> {
        self.inner
            .close()
            .map_err(|_| WebSocketError::CloseFailed.into())
    }
}

impl Drop for WebSocketWrapper {
    fn drop(&mut self) {
        self.inner.set_onopen(None);
        self.inner.set_onclose(None);
        self.inner.set_onerror(None);
        self.inner.set_onmessage(None);
        self.closed.store(true, Ordering::Release);
        self.close_event.notify(usize::MAX);
        let _ = self.inner.close();
    }
}
