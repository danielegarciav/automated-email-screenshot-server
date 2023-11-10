use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

pub fn init_tracing() {
  tracing_subscriber::registry()
    .with(
      tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "info,automation_test=debug,tower_http=debug,axum=debug".into()),
    )
    .with(
      tracing_subscriber::fmt::layer().event_format(tracing_subscriber::fmt::format().with_line_number(true)),
    )
    .init();
}
