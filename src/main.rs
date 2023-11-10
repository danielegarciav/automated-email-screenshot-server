mod app_result;
pub(crate) mod eml_task;
mod graceful_shutdown;
mod logging;
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
use tokio::sync::oneshot;
use tower_http::{
  cors::{self, CorsLayer},
  trace::TraceLayer,
};
use tracing::Instrument;

use eml_task::{EmlTaskManager, EmlTaskResult};
use logging::init_tracing;
use worker_thread::start_worker_thread;

use crate::eml_task::EmlTaskStatus;

#[derive(Clone)]
struct AppState {
  task_manager: Arc<EmlTaskManager>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
  init_tracing();
  let task_manager = Arc::new(EmlTaskManager::new());
  let worker_thread_task_manager = task_manager.clone();
  let worker = std::thread::spawn(move || start_worker_thread(worker_thread_task_manager));
  let state = AppState {
    task_manager: task_manager.clone(),
  };

  let app = Router::new()
    .route("/render-eml", post(handle_eml_render_request))
    .route("/render-eml-async", post(handle_async_eml_render_request))
    .route("/live-queue", get(handle_live_queue_request))
    .layer(TraceLayer::new_for_http().make_span_with(|request: &Request<_>| {
      let path = request.extensions().get::<MatchedPath>().map(MatchedPath::as_str);
      tracing::info_span!("request", method = ?request.method(), path)
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
  let (task_id, result_receiver) = match state.task_manager.enqueue_task(data.eml.clone()) {
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
  state
    .task_manager
    .report_task_status(&task_id, eml_task::EmlTaskStatus::Completed);
  Ok((headers, jpeg).into_response())
}

async fn continue_task_in_background(
  task_id: &str,
  result_receiver: oneshot::Receiver<EmlTaskResult>,
) -> anyhow::Result<()> {
  let eml_result = result_receiver.await?;
  let raw_image = eml_result?;
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
  tracing::info!("image encoded successfully! saving to disk...");
  tokio::fs::DirBuilder::new()
    .recursive(true)
    .create("output")
    .await?;
  tokio::fs::write(format!("output/{task_id}.jpg"), jpeg).await?;
  Ok(())
}

async fn handle_async_eml_render_request(
  State(state): State<AppState>,
  data: TypedMultipart<RenderEmlInput>,
) -> AppResult<Response> {
  tracing::info!("Length of eml is {} bytes", data.eml.len());
  let (task_id, result_receiver) = match state.task_manager.enqueue_task(data.eml.clone()) {
    Ok(x) => x,
    Err(err) => match err {
      eml_task::EmlTaskEnqueueError::TaskQueueFull => {
        return Ok((StatusCode::SERVICE_UNAVAILABLE).into_response())
      }
    },
  };

  tokio::spawn(
    async move {
      match continue_task_in_background(&task_id, result_receiver).await {
        Ok(..) => {
          tracing::info!("background task completed");
          state
            .task_manager
            .report_task_status(&task_id, EmlTaskStatus::Completed)
        }
        Err(err) => {
          tracing::error!("background task failed: {:?}", err);
          state
            .task_manager
            .report_task_status(&task_id, EmlTaskStatus::Failed)
        }
      }
    }
    .instrument(tracing::Span::current()),
  );
  Ok(().into_response())
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
