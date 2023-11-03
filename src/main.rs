#![allow(unused_attributes)]

pub(crate) mod eml_task;
pub(crate) mod worker_thread;

use axum::{
  extract::{MatchedPath, Multipart, State},
  http::{header, Request, StatusCode},
  response::{IntoResponse, Response},
  routing::post,
  Router,
};
use std::net::SocketAddr;
use tower_http::cors::{self, CorsLayer};

use tower_http::trace::TraceLayer;
use tracing::info_span;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use crate::worker_thread::start_worker_thread;
use eml_task::EmlTask;
use tokio::sync::{mpsc, oneshot};

#[derive(Clone)]
struct AppState {
  task_sender: mpsc::Sender<EmlTask>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
  tracing_subscriber::registry()
    .with(
      tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        // axum logs rejections from built-in extractors with the `axum::rejection`
        // target, at `TRACE` level. `axum::rejection=trace` enables showing those events
        "info,automation_test=debug,tower_http=debug,axum=debug".into()
      }),
    )
    .with(tracing_subscriber::fmt::layer())
    .init();

  let (task_sender, task_receiver) = mpsc::channel(12);
  let worker = std::thread::spawn(move || start_worker_thread(task_receiver));
  let state = AppState { task_sender };

  let app = Router::new()
    .route("/render-eml", post(render_eml))
    .layer(
      TraceLayer::new_for_http().make_span_with(|request: &Request<_>| {
        let path = request
          .extensions()
          .get::<MatchedPath>()
          .map(MatchedPath::as_str);

        info_span!(
            "request",
            method = ?request.method(),
            path,
        )
      }),
    )
    .layer(CorsLayer::new().allow_origin(cors::Any))
    .with_state(state);

  let addr = SocketAddr::from(([0, 0, 0, 0], 3001));
  tracing::info!("listening on http://{}", addr);

  axum::Server::bind(&addr)
    .serve(app.into_make_service())
    .await
    .unwrap();

  worker.join().unwrap().unwrap();
  Ok(())
}

async fn render_eml(State(state): State<AppState>, mut multipart: Multipart) -> Response {
  while let Some(field) = multipart.next_field().await.unwrap() {
    let name = field.name().unwrap().to_string();
    let data = field.bytes().await.unwrap();
    tracing::info!("Length of `{}` is {} bytes", name, data.len());
  }

  let (result_sender, result_receiver) = oneshot::channel();

  let task = EmlTask {
    eml_content: "".into(),
    response: result_sender,
    span: Some(tracing::Span::current()),
  };

  if let Err(err) = state.task_sender.try_send(task) {
    match err {
      mpsc::error::TrySendError::Full(..) => {
        return (StatusCode::SERVICE_UNAVAILABLE).into_response()
      }
      mpsc::error::TrySendError::Closed(..) => {
        return (StatusCode::INTERNAL_SERVER_ERROR).into_response()
      }
    }
  };

  let eml_result = match result_receiver.await {
    Ok(r) => r,
    Err(..) => return (StatusCode::INTERNAL_SERVER_ERROR).into_response(),
  };

  let raw_image = match eml_result {
    Ok(i) => i,
    Err(err) => {
      tracing::error!("{:?}", err);
      return (StatusCode::INTERNAL_SERVER_ERROR).into_response();
    }
  };

  let headers = [(header::CONTENT_TYPE, "image/jpeg")];

  tracing::info!("encoding image...");

  // method 1
  // pros: uses less memory (encoding output goes straight to network)
  // cons: takes longer (5 seconds)

  // let (image_send, image_receive) = tokio::io::duplex(usize::pow(2, 16));
  // let image_send_sync = tokio_util::io::SyncIoBridge::new(image_send);
  // let span = tracing::Span::current();
  // tokio::task::spawn_blocking(move || {
  //   let _span_guard = span.enter();
  //   let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(image_send_sync, 50);
  //   if let Err(err) = raw_image.write_with_encoder(encoder) {
  //     tracing::error!("failed to encode image, {:?}", err);
  //   } else {
  //     tracing::info!("image encoded successfully")
  //   }
  // });
  // let reader_stream = tokio_util::io::ReaderStream::new(image_receive);
  // let stream = reader_stream;
  // let body = StreamBody::new(stream);
  // (headers, body).into_response()

  // method 2
  // pros: faster (3 seconds)
  // cons: uses more memory (encoding output is entirely buffered before sending)

  let span = tracing::Span::current();
  let jpeg = tokio::task::spawn_blocking(move || {
    let _span_guard = span.enter();
    let mut buf = Vec::new();
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 50);
    raw_image.write_with_encoder(encoder).unwrap();
    buf
  })
  .await
  .unwrap();

  tracing::info!("image encoded successfully!");
  (headers, jpeg).into_response()
}
