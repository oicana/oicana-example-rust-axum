use std::time::Duration;

use axum::http::{Response, StatusCode};
use shutdown::shutdown_signal;
use tower_http::{
    compression::CompressionLayer,
    decompression::RequestDecompressionLayer,
    timeout::TimeoutLayer,
    trace::{DefaultMakeSpan, TraceLayer},
};
use tracing::Span;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use utoipa::OpenApi;
use utoipa_axum::router::OpenApiRouter;
use utoipa_swagger_ui::SwaggerUi;

mod blob;
mod certificate;
mod shutdown;
mod template;

const TEMPLATE_TAG: &str = "template";
const CERTIFICATE_TAG: &str = "certificates";
const BLOB_TAG: &str = "blob";

#[derive(OpenApi)]
#[openapi(
    external_docs(url = "https://oicana.com/docs/", description = "General documentation for Oicana."),
    tags(
        (name = TEMPLATE_TAG, description = "Template API endpoints. Find used templates at https://github.com/oicana/oicana-example-templates."),
        (name = CERTIFICATE_TAG, description = "Create certificates"),
        (name = BLOB_TAG, description = "Blob storage endpoints. Upload files (images, documents) to use as template inputs.")
    )
)]
struct ApiDoc;

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| format!("{}=trace", env!("CARGO_CRATE_NAME")).into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let (blob_router, blob_storage) = blob::router();

    // For simplicity, this example project will warm-up all templates on startup
    // all endpoints will expect templates to be in the cache
    let template_cache = std::sync::Arc::new(template::warmed_up_templates());

    let (router, api) = OpenApiRouter::with_openapi(ApiDoc::openapi())
        .nest(
            "/templates",
            template::router(blob_storage.clone(), template_cache.clone()),
        )
        .nest("/certificates", certificate::router(template_cache))
        .merge(blob_router)
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::default().include_headers(true))
                .on_response(|_response: &Response<_>, latency: Duration, _span: &Span| {
                    tracing::info!("Request took: {:?}", latency);
                }),
        )
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(1),
        ))
        .layer(RequestDecompressionLayer::new())
        .layer(CompressionLayer::new())
        .split_for_parts();

    let router = router.merge(SwaggerUi::new("/swagger").url("/api/openapi.json", api));

    let app = router.into_make_service();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000")
        .await
        .unwrap();
    tracing::debug!(
        "API docs at http://{}/swagger",
        listener.local_addr().unwrap()
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap();
    tracing::debug!("Server shut down!");
}
