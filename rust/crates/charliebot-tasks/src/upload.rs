//! This handler parses the request body before session lookup, then validates `file` before writing it.
//! It saves the client file's base name under `sessions/<sid>/uploads/` and returns the resolved path and byte count.

use std::ffi::OsStr;
use std::path::Path;
use std::sync::Arc;

use axum::extract::{FromRequestParts, Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use charliebot_compat::fastapi_form::{self, Field, FormModel};
use charliebot_compat::responses;
use charliebot_registry::FeatureContext;
use charliebot_registry::task_errors::task_http_error;
use tokio::io::{AsyncWriteExt, BufReader};

const UPLOAD_FIELDS: &[Field] = &[Field::file("file")];
const UPLOAD_FORM: FormModel = FormModel::new(UPLOAD_FIELDS);

/// Parse the body before session lookup, then validate and save the required file.
pub async fn upload_file(State(features): State<Arc<FeatureContext>>, request: axum::extract::Request) -> Response {
  let (mut parts, stream) = request.into_parts();
  let form = match fastapi_form::read(&parts.headers, stream).await {
    Ok(form) => form,
    Err(response) => return *response,
  };
  let AxumPath(session_id) = match AxumPath::<String>::from_request_parts(&mut parts, &features).await {
    Ok(session_id) => session_id,
    Err(response) => return response.into_response(),
  };
  if let Err(error) = features.store.require_session(&session_id).await {
    return task_http_error(error);
  }
  let config = match features.config.get() {
    Ok(config) => config,
    Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
  };
  if let Err(response) = form.validate(&UPLOAD_FORM) {
    return *response;
  }
  let file = form.file("file").expect("the required file field validated");
  let filename = file.filename().expect("a multipart file field has a filename");
  let filename_for_path = if filename.is_empty() { "upload" } else { filename };
  let basename = Path::new(filename_for_path).file_name().unwrap_or_else(|| OsStr::new(""));
  let uploads_dir = config.sessions_dir().join(&session_id).join("uploads");
  if let Err(error) = tokio::fs::create_dir_all(&uploads_dir).await {
    return responses::internal_error("OSError", error.to_string());
  }
  let destination = uploads_dir.join(basename);
  let size = match save_file(file.path(), &destination).await {
    Ok(size) => size,
    Err(error) => {
      tracing::warn!(
        target: "charliebot_tasks",
        session = %session_id,
        filename = %filename,
        error = %error,
        "file_upload_failed"
      );
      return responses::json(
        StatusCode::INTERNAL_SERVER_ERROR,
        &serde_json::json!({ "detail": "Failed to save uploaded file" }),
      );
    }
  };
  tracing::info!(
    target: "charliebot_tasks",
    session = %session_id,
    filename = %filename,
    size,
    "file_uploaded"
  );
  let path = match tokio::fs::canonicalize(&destination).await {
    Ok(path) => path,
    Err(error) => return responses::internal_error("OSError", error.to_string()),
  };
  responses::json(
    StatusCode::OK,
    &serde_json::json!({ "filename": filename, "path": path.to_string_lossy(), "size": size }),
  )
}

/// The copy's read buffer: 1 MiB, the chunk size the upload path has always copied with.
const COPY_BUFFER_BYTES: usize = 1024 * 1024;

async fn save_file(source: &Path, destination: &Path) -> std::io::Result<u64> {
  let mut source_file = BufReader::with_capacity(COPY_BUFFER_BYTES, tokio::fs::File::open(source).await?);
  let mut destination_file =
    tokio::fs::OpenOptions::new().create(true).truncate(true).write(true).open(destination).await?;
  let size = tokio::io::copy_buf(&mut source_file, &mut destination_file).await?;
  destination_file.flush().await?;
  Ok(size)
}
