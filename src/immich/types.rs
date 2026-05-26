//! Immich API response types.
//!
//! Field names match the camelCase JSON produced by Immich's REST API.
//! All unknown fields are silently ignored (lenient deserialization — no `deny_unknown_fields`).
//!
//! **Key design decisions based on API verification:**
//!
//! 1. `Asset.width` / `Asset.height` — top-level fields on every asset response; these are
//!    the actual rendered pixel dimensions and are the correct values to use for face-area
//!    percentage calculations (the EXIF `exifImageWidth/Height` are camera-sensor dimensions
//!    and can differ).
//!
//! 2. The smart-search response does NOT embed face bounding boxes inside each asset.
//!    `people[]` contains `PersonResponseDto` objects (id + name only).  Bounding boxes
//!    live in a separate `GET /api/faces?assetId={id}` endpoint → `Vec<AssetFace>`.
//!    Task 5 must fetch faces per asset before calling [`crate::filter::classify_candidate`].
//!
//! 3. There is no `unassignedFaces` field on the smart-search asset response.
//!    Unassigned faces (faces without a matched person) are returned by the same
//!    `GET /api/faces` endpoint with `face.person == null`.

use chrono::{DateTime, Utc};
use serde::Deserialize;

/// A single Immich library asset, as returned by smart-search results.
///
/// Only fields needed by the filter and caption pipeline are modelled.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Asset {
    /// Immich asset UUID.
    pub id: String,

    /// Timestamp the file was originally created (from EXIF or file metadata).
    /// Used for the caption prompt's `{date}` slot.
    pub file_created_at: Option<DateTime<Utc>>,

    /// Rendered pixel width of the asset.  Used (together with `height`) to
    /// compute face bounding-box area percentages.
    pub width: Option<u32>,

    /// Rendered pixel height of the asset.
    pub height: Option<u32>,

    /// EXIF metadata.  Optional — assets without parsed EXIF return `null`.
    pub exif_info: Option<ExifInfo>,

    /// Named people detected in this asset.  Each entry is a person Immich has
    /// recognised (given a name).  Bounding-box geometry is NOT included here;
    /// use `GET /api/faces?assetId={id}` for that.
    #[serde(default)]
    pub people: Vec<Person>,
}

/// EXIF metadata associated with an asset.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExifInfo {
    /// Camera-reported image width (may differ from rendered `Asset.width`).
    /// Present in the Immich API response for completeness; not used by the filter.
    #[allow(dead_code)]
    pub exif_image_width: Option<u32>,

    /// Camera-reported image height (may differ from rendered `Asset.height`).
    /// Present in the Immich API response for completeness; not used by the filter.
    #[allow(dead_code)]
    pub exif_image_height: Option<u32>,

    /// City name from EXIF GPS reverse-geocoding, or `null` if unavailable.
    pub city: Option<String>,

    /// Country name from EXIF GPS reverse-geocoding, or `null` if unavailable.
    pub country: Option<String>,

    /// Original capture timestamp from EXIF `DateTimeOriginal`.
    pub date_time_original: Option<DateTime<Utc>>,
}

/// A person (named individual) detected by Immich's ML pipeline.
///
/// This is `PersonResponseDto` from the Immich API — it carries identity
/// information only.  Spatial face data (bounding boxes) requires a separate
/// `GET /api/faces` call.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Person {
    /// Immich person UUID.
    #[allow(dead_code)]
    pub id: String,

    /// Human-assigned name, or empty string `""` if unrecognised.
    /// Immich returns `""` (not `null`) for unnamed people, so the filter
    /// checks `!name.is_empty()`.
    pub name: String,
}

/// A detected face, returned by `GET /api/faces?assetId={id}`.
///
/// One `AssetFace` per detected face region; `person` is `None` for
/// faces that Immich has not matched to a known person.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetFace {
    /// Immich face UUID.
    #[allow(dead_code)]
    pub id: String,

    /// Bounding box — left edge (pixels from left).
    pub bounding_box_x1: i32,
    /// Bounding box — top edge (pixels from top).
    pub bounding_box_y1: i32,
    /// Bounding box — right edge.
    pub bounding_box_x2: i32,
    /// Bounding box — bottom edge.
    pub bounding_box_y2: i32,

    /// Width of the image used by the face detector (may differ from asset width).
    #[allow(dead_code)]
    pub image_width: Option<u32>,
    /// Height of the image used by the face detector.
    #[allow(dead_code)]
    pub image_height: Option<u32>,

    /// Matched person, or `None` for unassigned faces.
    #[allow(dead_code)]
    pub person: Option<Person>,
}

impl AssetFace {
    /// Area of this bounding box in pixels², or 0 if the box is degenerate.
    pub fn bbox_area(&self) -> u64 {
        let w = (self.bounding_box_x2 - self.bounding_box_x1).max(0) as u64;
        let h = (self.bounding_box_y2 - self.bounding_box_y1).max(0) as u64;
        w * h
    }
}
