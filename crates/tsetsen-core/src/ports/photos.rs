use std::path::PathBuf;
use std::time::SystemTime;

use async_trait::async_trait;

use crate::{Ctx, ServiceError};

/// Where a photo was taken.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GeoPoint {
    /// Degrees north.
    pub latitude: f64,
    /// Degrees east.
    pub longitude: f64,
}

/// A photo in the user's library. Metadata only: no pixels.
#[derive(Clone, Debug, PartialEq)]
pub struct PhotoInfo {
    /// Stable within the library.
    pub id: String,
    /// When it was taken.
    pub taken: SystemTime,
    /// Where, if recorded.
    pub location: Option<GeoPoint>,
    /// Marked as a favorite.
    pub favorite: bool,
}

/// Photos taken in `[from, to)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhotoQuery {
    /// Inclusive start.
    pub from: SystemTime,
    /// Exclusive end.
    pub to: SystemTime,
    /// The most to return, newest first.
    pub limit: usize,
}

/// A photo copied out of the library, ready to attach or show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportedPhoto {
    /// The library id.
    pub id: String,
    /// A JPEG the caller may read and delete.
    pub path: PathBuf,
    /// Its size.
    pub bytes: u64,
}

/// The user's photo library. Still images only.
#[async_trait]
pub trait PhotoLibrary: Send + Sync {
    /// Photos matching `query`, newest first.
    async fn search(&self, query: &PhotoQuery, cx: &Ctx) -> Result<Vec<PhotoInfo>, ServiceError>;

    /// Copies `ids` out as JPEGs no larger than `max_pixels` on their longer
    /// side, in the order given.
    async fn export(
        &self,
        ids: &[String],
        max_pixels: u32,
        cx: &Ctx,
    ) -> Result<Vec<ExportedPhoto>, ServiceError>;
}

/// Puts pictures in front of the user, so they can see what the assistant
/// means.
#[async_trait]
pub trait Showcase: Send + Sync {
    /// Shows `pictures` together, labeled 1, 2, 3… in order, replacing
    /// whatever was shown before.
    async fn show(&self, pictures: &[PathBuf], cx: &Ctx) -> Result<(), ServiceError>;

    /// Takes them away again. Does nothing if nothing is shown.
    async fn hide(&self, cx: &Ctx) -> Result<(), ServiceError>;
}
