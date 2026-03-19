use std::{fs::File, sync::Arc};

use axum::{
    Json,
    body::Body,
    extract::{Path, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use dashmap::DashMap;
use oicana::{CompileError, Template};
use oicana_export::{pdf::export_merged_pdf, png::export_merged_png};
use oicana_files::packed::PackedTemplate;
use oicana_input::{
    CompilationConfig, TemplateInputs, input::blob::BlobInput as OicanaBlobInput,
    input::json::JsonInput as OicanaJsonInput,
};
use oicana_world::diagnostics::DiagnosticColor;
use serde::{Deserialize, Serialize};
use tokio_util::io::ReaderStream;
use tracing::{error, info};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;
use uuid::Uuid;

use crate::blob::{BlobStorage, get_blob};

const TEMPLATES: &[(&str, &str)] = &[
    ("accessibility", "0.1.0"),
    ("certificate", "0.1.0"),
    ("dependency", "0.1.0"),
    ("fonts", "0.1.0"),
    ("invoice", "0.1.0"),
    ("invoice_zugferd", "0.1.0"),
    ("minimal", "0.1.0"),
    ("table", "0.1.0"),
    ("multi_input", "0.1.0"),
];

type TemplateCache = Arc<DashMap<String, Template<PackedTemplate>>>;

#[derive(Clone)]
struct AppState {
    template_cache: TemplateCache,
    blob_storage: BlobStorage,
}

/// Create the template router with all template-related endpoints
pub fn router(blob_storage: BlobStorage, template_cache: TemplateCache) -> OpenApiRouter {
    let state = AppState {
        template_cache,
        blob_storage,
    };

    OpenApiRouter::new()
        .routes(routes!(compile_template))
        .routes(routes!(preview_template))
        .routes(routes!(reset_template))
        .routes(routes!(get_template))
        .routes(routes!(get_template_list))
        .with_state(state)
}

/// Load and cache all templates.
/// This method expects templates to compile in development mode without extra inputs.
pub fn warmed_up_templates() -> DashMap<String, Template<PackedTemplate>> {
    let cache = DashMap::new();

    for (id, version) in TEMPLATES {
        let template_file = match File::open(format!("templates/{id}-{version}.zip")) {
            Ok(file) => file,
            Err(error) => {
                error!("'templates/{id}-{version}.zip' not found during warm-up: {error:?}");
                continue;
            }
        };
        let mut template = match Template::init(template_file) {
            Ok(template) => template,
            Err(error) => {
                error!(
                    "'templates/{id}-{version}.zip' failed to compile during warm-up: {error:?}"
                );
                continue;
            }
        };
        template.set_diagnostic_color(DiagnosticColor::None);
        info!("Warmed-up {id} v{version}.");
        cache.insert(id.to_string(), template);
    }

    cache
}

enum TemplateError {
    NotFound(String),
    BlobNotFound { template_id: String, blob_id: Uuid },
    CompilationFailure { id: String, error: CompileError },
    ExportFailure { id: String, error: String },
}

impl IntoResponse for TemplateError {
    fn into_response(self) -> Response {
        #[derive(Serialize)]
        struct ErrorResponse {
            message: String,
        }

        let (status, message) = match self {
            TemplateError::NotFound(template_id) => {
                tracing::error!(%template_id, "Template '{template_id}' not found!");
                (
                    StatusCode::NOT_FOUND,
                    format!("Template '{template_id}' not found!"),
                )
            }
            TemplateError::BlobNotFound {
                template_id,
                blob_id,
            } => {
                tracing::error!(%template_id, %blob_id, "Blob with id {blob_id} not found for template '{template_id}'");
                (
                    StatusCode::BAD_REQUEST,
                    format!(
                        "Blob with id {blob_id} not found. Please use an ID of a blob that was previously uploaded."
                    ),
                )
            }
            TemplateError::CompilationFailure {
                id: template_id,
                error,
            } => match error {
                CompileError::ValidationFailed(validation_error) => {
                    tracing::error!(%template_id, "Template '{template_id}' received invalid input: {validation_error}");
                    (
                        StatusCode::BAD_REQUEST,
                        format!(
                            "Template '{template_id}' received invalid input: {validation_error}"
                        ),
                    )
                }
                CompileError::CompilationFailed(compilation_failure) => {
                    match compilation_failure.warnings {
                        Some(ref warnings) => {
                            tracing::error!(%template_id, "Template '{template_id}' failed to compile with given inputs: {}{}", compilation_failure.error, warnings)
                        }
                        None => {
                            tracing::error!(%template_id, "Template '{template_id}' failed to compile with given inputs: {}", compilation_failure.error)
                        }
                    }
                    (
                        StatusCode::BAD_REQUEST,
                        format!(
                            "Template '{template_id}' failed to compile with given inputs: {}{}",
                            compilation_failure.error,
                            compilation_failure
                                .warnings
                                .map(|warning| format!("\n\n{warning}"))
                                .unwrap_or(String::new())
                        ),
                    )
                }
            },
            TemplateError::ExportFailure {
                id: template_id,
                error,
            } => {
                tracing::error!(%template_id, %error, "Template '{template_id}' failed to export: {error}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Template '{template_id}' failed to export!\n{error}"),
                )
            }
        };

        (status, Json(ErrorResponse { message })).into_response()
    }
}

#[utoipa::path(
    method(post),
    tag = super::TEMPLATE_TAG,
    path = "/{template_id}/compile",
    params(("template_id" = String, example = "table", description = "The identifier of the template to compile.")),
    request_body(content = CompilationPayload, description = "Inputs and config for template compilation", content_type = "application/json"),
    description = "Compile a template with given inputs.",
    responses(
        (status = OK, description = "Success", content_type = "application/pdf")
    )
)]
#[axum::debug_handler]
async fn compile_template(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(payload): Json<CompilationPayload>,
) -> impl IntoResponse {
    let Some(mut template) = state.template_cache.get_mut(&id) else {
        return Err(TemplateError::NotFound(id));
    };

    let mut inputs = TemplateInputs::new();
    inputs.with_config(CompilationConfig::production());

    for JsonInput { key, value } in payload.json_inputs {
        inputs.with_input(OicanaJsonInput::new(key, value.to_string()));
    }

    for BlobInput { key, blob_id } in payload.blob_inputs {
        if let Some(data) = get_blob(&state.blob_storage, blob_id) {
            inputs.with_input(OicanaBlobInput::new(key, data));
        } else {
            return Err(TemplateError::BlobNotFound {
                template_id: id,
                blob_id,
            });
        }
    }

    let compilation_result = match template.compile(inputs) {
        Ok(document) => document,
        Err(error) => return Err(TemplateError::CompilationFailure { id, error }),
    };

    let pdf = match export_merged_pdf(
        &compilation_result.document,
        &*template,
        &template.manifest().tool.oicana.export.pdf.standards,
    ) {
        Ok(pdf) => pdf,
        Err(error) => return Err(TemplateError::ExportFailure { id, error }),
    };
    let body = Body::from(pdf);

    let headers = [
        (header::CONTENT_TYPE, "application/pdf".to_owned()),
        (
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{id}.pdf\""),
        ),
    ];

    Ok((headers, body))
}

#[utoipa::path(
    method(post),
    tag = super::TEMPLATE_TAG,
    path = "/{template_id}/preview",
    params(("template_id" = String, example = "table", description = "The identifier of the template to preview.")),
    request_body(content = CompilationPayload, description = "Inputs and config for template compilation", content_type = "application/json"),
    description = "Generate a PNG preview of the template with given inputs.",
    responses(
        (status = OK, description = "Success", content_type = "image/png")
    )
)]
#[axum::debug_handler]
async fn preview_template(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(payload): Json<CompilationPayload>,
) -> impl IntoResponse {
    let Some(mut template) = state.template_cache.get_mut(&id) else {
        return Err(TemplateError::NotFound(id));
    };

    let mut inputs = TemplateInputs::new();
    inputs.with_config(CompilationConfig::development());

    for JsonInput { key, value } in payload.json_inputs {
        inputs.with_input(OicanaJsonInput::new(key, value.to_string()));
    }

    for BlobInput { key, blob_id } in payload.blob_inputs {
        if let Some(data) = get_blob(&state.blob_storage, blob_id) {
            inputs.with_input(OicanaBlobInput::new(key, data));
        } else {
            return Err(TemplateError::BlobNotFound {
                template_id: id,
                blob_id,
            });
        }
    }

    let compilation_result = match template.compile(inputs) {
        Ok(document) => document,
        Err(error) => return Err(TemplateError::CompilationFailure { id, error }),
    };

    // Export all pages merged as PNG
    let png = export_merged_png(&compilation_result.document, 1.0).unwrap();
    let body = Body::from(png);

    let headers = [
        (header::CONTENT_TYPE, "image/png".to_owned()),
        (
            header::CONTENT_DISPOSITION,
            format!("inline; filename=\"{id}.png\""),
        ),
    ];

    Ok((headers, body))
}

#[utoipa::path(
    method(post),
    tag = super::TEMPLATE_TAG,
    path = "/{template_id}/reset",
    params(("template_id" = String, example = "table", description = "The identifier of the template to reset.")),
    description = "Reset (remove) a template from the cache. The template will be reloaded on next use.",
    responses(
        (status = NO_CONTENT, description = "Template successfully removed from cache"),
        (status = NOT_FOUND, description = "Template not found in cache")
    )
)]
async fn reset_template(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.template_cache.remove(&id) {
        Some(_) => {
            info!("Template '{}' removed from cache", id);
            StatusCode::NO_CONTENT
        }
        None => {
            error!("Template '{}' not found in cache", id);
            StatusCode::NOT_FOUND
        }
    }
}

#[utoipa::path(
    method(get),
    tag = super::TEMPLATE_TAG,
    path = "/{template_id}",
    params(("template_id" = String, example = "table", description = "The identifier of the template to download.")),
    description = "Download a packed template.",
    responses(
        (status = OK, description = "Success", content_type = "application/zip")
    )
)]
async fn get_template(Path(id): Path<String>) -> impl IntoResponse {
    let file = match tokio::fs::File::open(format!("templates/{id}-0.1.0.zip")).await {
        Ok(file) => file,
        Err(err) => {
            return Err((
                StatusCode::NOT_FOUND,
                format!("Template not found: {}", err),
            ));
        }
    };

    let stream = ReaderStream::new(file);
    let body = Body::from_stream(stream);

    let headers = [
        (header::CONTENT_TYPE, "application/zip".to_owned()),
        (
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{id}.zip\""),
        ),
    ];

    Ok((headers, body))
}

/// A list of template IDs known by the service
#[derive(ToSchema, Serialize)]
struct TemplateList(Vec<&'static str>);

#[utoipa::path(
    method(get),
    tag = super::TEMPLATE_TAG,
    path = "",
    description = "Get a list of all template IDs known to the service.",
    responses(
        (status = OK, description = "Success", body = TemplateList, content_type = "application/json")
    )
)]
async fn get_template_list() -> impl IntoResponse {
    Json(TemplateList(
        TEMPLATES
            .iter()
            .map(|(id, _version)| id.to_owned())
            .collect(),
    ))
}

#[derive(ToSchema, Deserialize)]
#[schema(example = json!({
    "jsonInputs": [
        {
            "key": "data",
            "value": {
                "description": "from sample data",
                "rows": [
                    {
                        "name": "Frank",
                        "one": "first",
                        "two": "second",
                        "three": "third"
                    },
                    {
                        "name": "John",
                        "one": "first_john",
                        "two": "second_john",
                        "three": "third_john"
                    }
                ]
            }
        }
    ],
    "blobInputs": [
        {
            "key": "logo",
            "blobId": "00000000-0000-0000-0000-000000000000"
        }
    ]
}))]
struct CompilationPayload {
    #[serde(rename = "jsonInputs")]
    json_inputs: Vec<JsonInput>,
    #[serde(default, rename = "blobInputs")]
    blob_inputs: Vec<BlobInput>,
}

#[derive(ToSchema, Deserialize)]
#[schema(example = json!({"key": "data", "value": { "test": "example content", "items": [ { "name": "Frank", "one": "A", "two": "C", "three": "A" }, { "name": "John", "one": "C", "two": "no show", "three": "B" } ] } }))]
struct JsonInput {
    key: String,
    value: serde_json::Value,
}

#[derive(ToSchema, Deserialize)]
#[schema(example = json!({"key": "logo", "blobId": "00000000-0000-0000-0000-000000000000"}))]
struct BlobInput {
    /// The input key for the blob
    key: String,
    /// UUID of the blob from the blob storage
    #[serde(rename = "blobId")]
    blob_id: Uuid,
}
