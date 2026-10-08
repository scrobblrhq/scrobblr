//! Uploaded images: what we make of a file before storing it.

pub mod image;

use tokio::sync::Semaphore;

use crate::errors::AppError;
use image::{MAX_SOURCE_PIXELS, MAX_SOURCE_SIDE, Rejected};

/// Decodes running at once: each may hold a few hundred MB.
static DECODES: Semaphore = Semaphore::const_new(2);

/// [`image::normalize`] on the blocking pool, a bounded number at a time.
pub async fn normalize(bytes: Vec<u8>, max_side: u32) -> Result<Vec<u8>, AppError> {
    let _permit = DECODES
        .acquire()
        .await
        .map_err(|e| AppError::Internal(e.into()))?;
    tokio::task::spawn_blocking(move || image::normalize(&bytes, max_side))
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("image task failed: {e}")))?
        .map_err(|rejected| match rejected {
            Rejected::NotAnImage => {
                AppError::BadRequest("the file is not a JPEG, PNG or WebP image".into())
            }
            Rejected::TooLarge => AppError::BadRequest(format!(
                "images can be at most {MAX_SOURCE_SIDE} px a side and {} megapixels",
                MAX_SOURCE_PIXELS / 1_000_000
            )),
        })
}
