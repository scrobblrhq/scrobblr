//! Uploaded images: what we make of a file, and where it is kept.
//!
//! Handlers store and delete uploads through [`Media`] and only ever see
//! keys; the database keeps the key and `shared::media` turns it into a
//! URL. Another backend (an S3-compatible bucket) is another [`Storage`]
//! variant, with no change to handlers or stored data.

pub mod image;
mod local;

use std::path::Path;

use shared::media::{UploadKey, UploadKind};
use tokio::sync::Semaphore;

use crate::errors::AppError;
use image::{MAX_SOURCE_PIXELS, MAX_SOURCE_SIDE, Rejected};
pub use local::LocalStorage;

/// Decodes running at once: each may hold a few hundred MB.
static DECODES: Semaphore = Semaphore::const_new(2);

#[derive(Debug)]
pub enum Storage {
    Local(LocalStorage),
}

impl Storage {
    async fn put(&self, key: &UploadKey, bytes: Vec<u8>) -> std::io::Result<()> {
        match self {
            Self::Local(local) => local.put(key, bytes).await,
        }
    }

    async fn delete(&self, key: &UploadKey) -> std::io::Result<()> {
        match self {
            Self::Local(local) => local.delete(key).await,
        }
    }
}

#[derive(Debug)]
pub struct Media {
    storage: Storage,
}

impl Media {
    pub fn new(storage: Storage) -> Self {
        Self { storage }
    }

    /// The directory the API serves at `/uploads`, if uploads are on disk.
    pub fn local_root(&self) -> Option<&Path> {
        match &self.storage {
            Storage::Local(local) => Some(local.root()),
        }
    }

    /// Normalizes an uploaded file and stores it under a new key.
    pub async fn store(&self, kind: UploadKind, raw: Vec<u8>) -> Result<UploadKey, AppError> {
        let bytes = normalize(raw, max_side(kind)).await?;
        let key = UploadKey::generate(kind);
        self.storage.put(&key, bytes).await.map_err(|e| {
            AppError::Internal(anyhow::anyhow!("could not store upload {key}: {e}"))
        })?;
        Ok(key)
    }

    /// Best-effort: a failure is logged, never returned.
    pub async fn delete(&self, key: &UploadKey) {
        if let Err(e) = self.storage.delete(key).await {
            tracing::warn!(%key, "could not delete upload: {e}");
        }
    }

    /// Deletes a user's previous avatar once their `image_url` no longer
    /// holds it. Only keys the avatar upload created are deleted: an
    /// external URL isn't ours, and a key from before kinds existed may
    /// have been converted from a URL the user typed.
    pub async fn delete_replaced_avatar(&self, previous: Option<&str>, current: Option<&str>) {
        let Some(previous) = previous.filter(|&p| Some(p) != current) else {
            return;
        };
        if let Some(key) = UploadKey::parse(previous)
            && key.kind() == Some(UploadKind::Avatar)
        {
            self.delete(&key).await;
        }
    }
}

fn max_side(kind: UploadKind) -> u32 {
    match kind {
        UploadKind::Avatar => 512,
        UploadKind::ArtistImage | UploadKind::AlbumImage => 1024,
    }
}

/// [`image::normalize`] on the blocking pool, a bounded number at a time.
async fn normalize(bytes: Vec<u8>, max_side: u32) -> Result<Vec<u8>, AppError> {
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
