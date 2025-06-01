use std::sync::Arc;

use futures_util::lock::Mutex;
use regex::Regex;
use tokio_util::compat::FuturesAsyncReadCompatExt;
use wisp_mux::{
	packet::{CloseReason, ConnectPacket, StreamType},
	stream::MuxStream,
	ws::TransportWrite,
	ServerMux, WispError,
};
use worker::{
	console_log, console_warn, event, wasm_bindgen_futures::spawn_local,
	worker_sys::web_sys::WebSocket, Context, Env, Error, Headers, Request, Response, ResponseBody,
	Result, Socket, WebSocketPair,
};
use ws_wrapper::WebSocketWrapper;
mod ws_wrapper;

fn to_worker(val: WispError) -> Error {
	Error::RustError(val.to_string())
}

async fn stream_handler(
	packet: ConnectPacket,
	stream: MuxStream<impl TransportWrite>,
) -> Result<()> {
	match packet.stream_type {
		StreamType::Tcp => {
			let mut socket = Socket::builder().connect(packet.host, packet.port)?;
			let mut wisp_socket = stream.into_async_rw().compat();
			tokio::io::copy_bidirectional(&mut socket, &mut wisp_socket).await?;
		}
		StreamType::Udp | StreamType::Other(_) => stream
			.close(CloseReason::ServerStreamInvalidInfo)
			.await
			.map_err(to_worker)?,
	}
	Ok(())
}

async fn ws_handler(ws: WebSocket, env: Env) -> Result<()> {
	let (tx, rx) = WebSocketWrapper::connect(ws);

	console_log!("Accepted websocket");

	let (server, fut) = ServerMux::new(rx.into_read(), tx.into_write(), 128, None)
		.await
		.map_err(to_worker)?
		.with_no_required_extensions();

	console_log!("Created mux");

	spawn_local(async move {
		console_warn!("Mux future result: {:?}", fut.await);
	});

	let connected_vec = Arc::new(Mutex::new(Vec::new()));

	let allowed_host_regex = env.var("allowed_host_regex").ok().and_then(|x| {
		let regex = Regex::try_from(x.to_string()).ok();
		if regex.is_none() {
			console_warn!("allowed_host_regex was invalid. continuing without allowed_host_regex");
		}
		regex
	});

	while let Some((packet, stream)) = server.wait_for_stream().await {
		let connected_vec = connected_vec.clone();
		let stream_ident = (packet.host.clone(), packet.port);
		let mut locked = connected_vec.lock().await;
		if locked.contains(&stream_ident) || locked.len() >= 6 {
			stream
				.close(CloseReason::ServerStreamThrottled)
				.await
				.map_err(to_worker)?;
			console_warn!("Blocked stream creation because one stream for that combo was already created or too many sockets were created");
			continue;
		}
		if let Some(ref regex) = allowed_host_regex {
			if !regex.is_match(&packet.host) {
				stream
					.close(CloseReason::ServerStreamBlockedAddress)
					.await
					.map_err(to_worker)?;
			}
		}
		locked.push(stream_ident.clone());
		drop(locked);
		console_log!("Created stream");
		spawn_local(async move {
			console_warn!(
				"Stream handle future result: {:?}",
				stream_handler(packet, stream).await
			);
			connected_vec.lock().await.retain(|x| *x != stream_ident);
		})
	}

	Ok(())
}

#[event(fetch)]
async fn main(req: Request, env: Env, _ctx: Context) -> Result<Response> {
	if req.headers().get("Upgrade")?.unwrap_or_default() != "websocket" {
		return Ok(
			Response::from_body(ResponseBody::Body(include_bytes!("426.jpg").to_vec()))?
				.with_status(426)
				.with_headers(Headers::from_iter(
					[("Content-Type", "image/jpeg")].into_iter(),
				)),
		);
	}

	let pair = WebSocketPair::new()?;
	pair.server.accept()?;
	let server = pair.server;

	spawn_local(async move {
		let _ = ws_handler(server.as_ref().clone(), env).await;
	});

	Response::from_websocket(pair.client)
}
