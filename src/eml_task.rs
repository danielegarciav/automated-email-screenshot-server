pub struct EmlTask {
  pub eml_content: axum::body::Bytes,
  pub response: tokio::sync::oneshot::Sender<EmlTaskResult>,
  pub span: Option<tracing::Span>,
}

pub type EmlTaskResult = anyhow::Result<image::DynamicImage>;
