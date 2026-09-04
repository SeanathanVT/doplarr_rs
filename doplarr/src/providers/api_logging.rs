/// Shared utilities for logging API errors from generated OpenAPI clients
///
/// The generated clients each define their own error enum, but they share the same
/// shape. This module provides generic logging for these errors.
use tracing::error;

/// Generic implementation for logging HTTP error responses
/// This works for every backend's generated `apis::Error`
pub fn log_api_error_details(status: reqwest::StatusCode, content: &str, context: &str) {
    error!("{} - HTTP {}: {}", context, status, content);
}
