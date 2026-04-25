use std::sync::Arc;

use axum::{
    Json,
    body::Body,
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use dashmap::DashMap;
use oicana::{
    CompileError, Template,
    export::pdf::export_merged_pdf,
    files::packed::PackedTemplate,
    input::{CompilationConfig, TemplateInputs, input::json::JsonInput as OicanaJsonInput},
};
use serde::{Deserialize, Serialize};
use tracing::error;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

type TemplateCache = Arc<DashMap<String, Template<PackedTemplate>>>;

#[derive(Clone)]
struct AppState {
    template_cache: TemplateCache,
}

pub fn router(template_cache: TemplateCache) -> OpenApiRouter {
    let state = AppState { template_cache };

    OpenApiRouter::new()
        .routes(routes!(create_certificate))
        .with_state(state)
}

enum CertificateError {
    TemplateNotFound,
    SerializationFailure(String),
    CompilationFailure(CompileError),
    ExportFailure(String),
}

impl IntoResponse for CertificateError {
    fn into_response(self) -> Response {
        #[derive(Serialize)]
        struct ErrorResponse {
            message: String,
        }

        let (status, message) = match self {
            CertificateError::TemplateNotFound => {
                error!("Certificate template not found!");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Certificate template not found!".to_string(),
                )
            }
            CertificateError::SerializationFailure(error) => {
                error!(%error, "Failed to serialize certificate input");
                (
                    StatusCode::BAD_REQUEST,
                    format!("Failed to serialize input: {error}"),
                )
            }
            CertificateError::CompilationFailure(error) => match error {
                CompileError::ValidationFailed(validation_error) => {
                    error!("Certificate input validation failed: {validation_error}");
                    (
                        StatusCode::BAD_REQUEST,
                        format!("Certificate input validation failed: {validation_error}"),
                    )
                }
                CompileError::CompilationFailed(compilation_failure) => {
                    match compilation_failure.warnings {
                        Some(ref warnings) => {
                            error!(
                                "Certificate template failed to compile: {}{}",
                                compilation_failure.error, warnings
                            )
                        }
                        None => {
                            error!(
                                "Certificate template failed to compile: {}",
                                compilation_failure.error
                            )
                        }
                    }
                    (
                        StatusCode::BAD_REQUEST,
                        format!(
                            "Failed to compile certificate: {}{}",
                            compilation_failure.error,
                            compilation_failure
                                .warnings
                                .map(|warning| format!("\n\n{warning}"))
                                .unwrap_or(String::new())
                        ),
                    )
                }
            },
            CertificateError::ExportFailure(error) => {
                error!(%error, "Certificate failed to export");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to export certificate: {error}"),
                )
            }
        };

        (status, Json(ErrorResponse { message })).into_response()
    }
}

/// Payload to create a certificate
#[derive(ToSchema, Serialize, Deserialize)]
#[schema(example = json!({"name": "Jane Doe"}))]
struct CreateCertificate {
    /// Name to create the certificate for
    #[schema(example = "Jane Doe")]
    name: String,
}

#[utoipa::path(
    method(post),
    tag = super::CERTIFICATE_TAG,
    path = "",
    request_body(content = CreateCertificate, description = "Certificate details", content_type = "application/json"),
    description = "Create a certificate",
    responses(
        (status = OK, description = "The compiled PDF certificate", content_type = "application/pdf")
    )
)]
#[axum::debug_handler]
async fn create_certificate(
    State(state): State<AppState>,
    Json(request): Json<CreateCertificate>,
) -> Result<impl IntoResponse, CertificateError> {
    let template_id = "certificate";
    let Some(mut template) = state.template_cache.get_mut(template_id) else {
        return Err(CertificateError::TemplateNotFound);
    };

    let mut inputs = TemplateInputs::new();
    inputs.with_config(CompilationConfig::production());

    // Serialize the typed input to JSON and pass it with the key "certificate"
    // This matches the template's expected input key
    // See https://github.com/oicana/oicana-example-templates/blob/672967c5b667dfa845228cac443d32b8b3c7ae0a/templates/certificate/typst.toml#L12
    let json_value = serde_json::to_value(&request)
        .map_err(|e| CertificateError::SerializationFailure(e.to_string()))?;

    inputs.with_input(OicanaJsonInput::new(
        "certificate".to_string(),
        json_value.to_string(),
    ));

    let compilation_result = template
        .compile(inputs)
        .map_err(CertificateError::CompilationFailure)?;

    let pdf = export_merged_pdf(
        &compilation_result.document,
        &*template,
        &template.manifest().tool.oicana.export.pdf.standards,
    )
    .map_err(CertificateError::ExportFailure)?;

    let body = Body::from(pdf);

    let headers = [
        (header::CONTENT_TYPE, "application/pdf".to_owned()),
        (
            header::CONTENT_DISPOSITION,
            "attachment; filename=\"certificate.pdf\"".to_owned(),
        ),
    ];

    Ok((headers, body))
}
