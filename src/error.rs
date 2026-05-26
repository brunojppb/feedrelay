use actix_web::HttpResponse;
use thiserror::Error;

/// Application-level errors. Extend as new tasks add variants.
#[allow(dead_code)]
#[derive(Debug, Error)]
pub enum AppError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
}

impl actix_web::ResponseError for AppError {
    fn error_response(&self) -> HttpResponse {
        match self {
            AppError::Database(_) => HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "database_error"
            })),
        }
    }
}
