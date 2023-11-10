mod app_result;
pub(crate) mod eml_task;
mod graceful_shutdown;
pub(crate) mod worker_thread;

use app_result::AppResult;

use axum::{
  body::Bytes,
  extract::{
    ws::{Message, WebSocket, WebSocketUpgrade},
    MatchedPath, State,
  },
  http::{header, Request, StatusCode},
  response::{IntoResponse, Response},
  routing::{get, post},
  Router,
};
use axum_typed_multipart::{TryFromMultipart, TypedMultipart};
use std::{net::SocketAddr, sync::Arc};
use tower_http::cors::{self, CorsLayer};

use tower_http::trace::TraceLayer;
use tracing::info_span;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use crate::worker_thread::start_worker_thread;
use eml_task::EmlTaskManager;

#[derive(Clone)]
struct AppState {
  task_manager: Arc<EmlTaskManager>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
  tracing_subscriber::registry()
    .with(
      tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "info,automation_test=debug,tower_http=debug,axum=debug".into()),
    )
    .with(
      tracing_subscriber::fmt::layer().event_format(tracing_subscriber::fmt::format().with_line_number(true)),
    )
    .init();

  let task_manager = Arc::new(EmlTaskManager::new());
  let worker_thread_task_manager = task_manager.clone();
  let worker = std::thread::spawn(move || start_worker_thread(worker_thread_task_manager));
  let state = AppState {
    task_manager: task_manager.clone(),
  };

  let app = Router::new()
    .route("/render-eml", post(handle_eml_render_request))
    .route("/live-queue", get(handle_live_queue_request))
    .layer(TraceLayer::new_for_http().make_span_with(|request: &Request<_>| {
      let path = request.extensions().get::<MatchedPath>().map(MatchedPath::as_str);
      info_span!("request", method = ?request.method(), path)
    }))
    .layer(CorsLayer::new().allow_origin(cors::Any))
    .with_state(state);

  let addr = SocketAddr::from(([0, 0, 0, 0], 3001));
  tracing::info!("listening on http://{}", addr);

  axum::Server::bind(&addr)
    .serve(app.into_make_service())
    .with_graceful_shutdown(graceful_shutdown::shutdown_signal())
    .await
    .unwrap();

  task_manager.signal_shutdown();
  worker.join().unwrap().unwrap();
  Ok(())
}

#[derive(TryFromMultipart)]
struct RenderEmlInput {
  eml: Bytes,
}

async fn handle_eml_render_request(
  State(state): State<AppState>,
  data: TypedMultipart<RenderEmlInput>,
) -> AppResult<Response> {
  tracing::info!("Length of eml is {} bytes", data.eml.len());
  let result_receiver = match state.task_manager.enqueue_task(data.eml.clone()) {
    Ok(x) => x,
    Err(err) => match err {
      eml_task::EmlTaskEnqueueError::TaskQueueFull => {
        return Ok((StatusCode::SERVICE_UNAVAILABLE).into_response())
      }
    },
  };
  let eml_result = result_receiver.await?;
  let raw_image = eml_result?;
  let headers = [(header::CONTENT_TYPE, "image/jpeg")];

  tracing::info!("encoding image...");
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
  Ok((headers, jpeg).into_response())
}

async fn handle_live_queue_request(State(state): State<AppState>, ws: WebSocketUpgrade) -> Response {
  ws.on_upgrade(|socket| handle_live_queue_socket(socket, state))
}

async fn handle_live_queue_socket(mut socket: WebSocket, state: AppState) {
  let mut receiver = state.task_manager.subscribe_to_updates();
  loop {
    match receiver.recv().await {
      Ok(update) => {
        let message = serde_json::to_string(&update).unwrap_or("failed to serialize event".to_string());
        if socket.send(Message::Text(message)).await.is_err() {
          break;
        }
      }
      Err(err) => {
        let _ = socket.send(Message::Text(format!("error: {err:?}"))).await;
        break;
      }
    };
  }
  _ = socket.close().await;
}
