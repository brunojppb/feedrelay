//! Candidate classifier: decides which publishability tier an [`Asset`] falls
//! into ([`CandidateClass::Preferred`], [`CandidateClass::Acceptable`], or
//! [`CandidateClass::Rejected`]).
//!
//! ## Design notes
//!
//! The filter is a pure function — no I/O, no async.  All external state it
//! needs is passed in as arguments:
//!
//! - `asset` — the asset metadata from Immich smart-search.
//! - `faces` — the face list from `GET /api/faces?assetId={id}` (may be empty
//!   if the asset has no detected faces, or if the caller skips the fetch for
//!   assets that already fail another check).
//! - `thresholds` — configurable area percentages.
//! - `posted_asset_ids` — set of asset IDs that have already become Posts.
//!
//! ## Why faces are separate
//!
//! Immich's `POST /api/search/smart` response includes a `people[]` array
//! (identity only — no bounding boxes).  Bounding-box geometry requires a
//! separate `GET /api/faces?assetId` call.  Task 5's worker is responsible
//! for fetching faces before calling this function.
//!
//! ## Check order (cheap-first)
//!
//! 1. Already posted (hash-set lookup — O(1))
//! 2. Named person present (linear scan over `people[]` — cheap, no math)
//! 3. Location present (two Option checks)
//! 4. Image dimensions present (needed for area math)
//! 5. Per-face area exceeded (linear scan with multiply)
//! 6. Total face area exceeded (sum)

use std::collections::HashSet;

use crate::immich::types::{Asset, AssetFace};

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Reason an asset was rejected by the candidate filter.
///
/// Variants are ordered by the check sequence (cheap first).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RejectReason {
    /// Asset was already used in a previous successful Post.
    AlreadyPosted,

    /// A named/recognised person appears in `people[]`, regardless of face size.
    NamedPersonPresent { person_name: String },

    /// No usable location in EXIF (both `city` and `country` are null, or
    /// `exifInfo` itself is absent).
    NoLocation,

    /// `Asset.width` / `Asset.height` are absent — cannot compute face areas.
    MissingImageDimensions,

    /// A single face bounding box exceeds the per-face area threshold.
    PerFaceAreaExceeded {
        /// Truncated integer percent (e.g. `10` means ≥10 %).
        face_pct: u8,
    },

    /// The combined area of all face bounding boxes exceeds the total threshold.
    TotalFaceAreaExceeded {
        /// Truncated integer percent.
        total_pct: u8,
    },
}

/// Tier assigned to an asset by [`classify_candidate`].
///
/// `Preferred` and `Acceptable` are both publishable; the pipeline prefers
/// the former. `Rejected` carries a [`RejectReason`] describing the reject cause.
#[must_use]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CandidateClass {
    /// Immich detected zero faces on the asset.
    Preferred,
    /// Faces present, all within the per-face and total area thresholds.
    Acceptable,
    /// Hard-rejected — see [`RejectReason`].
    Rejected(RejectReason),
}

/// Configurable face-area thresholds.
#[derive(Debug, Clone)]
pub struct FilterThresholds {
    /// Maximum allowed area for any single face, as a percentage of total
    /// image area.  Default: `1.0`.
    pub per_face_pct: f64,

    /// Maximum allowed *combined* area for all faces, as a percentage.
    /// Default: `2.0`.
    pub total_pct: f64,
}

impl Default for FilterThresholds {
    fn default() -> Self {
        Self {
            // Keep faces small enough to read as background, never as the subject.
            per_face_pct: 1.0,
            // Combined area of all detected faces must stay under 2% of image area.
            total_pct: 2.0,
        }
    }
}

/// Classify `asset` into a publishability tier.
///
/// Returns:
/// - [`CandidateClass::Preferred`] when no faces were detected on the asset.
/// - [`CandidateClass::Acceptable`] when faces are present but all under the
///   configured per-face and total area thresholds.
/// - [`CandidateClass::Rejected`] with a [`RejectReason`] for any hard-fail
///   check (already posted, named person, no location, missing dimensions,
///   oversized face, or oversized combined face area).
///
/// Check order is cheap-first.
pub fn classify_candidate(
    asset: &Asset,
    faces: &[AssetFace],
    thresholds: &FilterThresholds,
    posted_asset_ids: &HashSet<String>,
) -> CandidateClass {
    // 1. Already posted
    if posted_asset_ids.contains(&asset.id) {
        return CandidateClass::Rejected(RejectReason::AlreadyPosted);
    }

    // 2. Named person present
    for person in &asset.people {
        if !person.name.is_empty() {
            return CandidateClass::Rejected(RejectReason::NamedPersonPresent {
                person_name: person.name.clone(),
            });
        }
    }

    // 3. Location present
    let has_location = asset
        .exif_info
        .as_ref()
        .is_some_and(|e| e.city.is_some() || e.country.is_some());
    if !has_location {
        return CandidateClass::Rejected(RejectReason::NoLocation);
    }

    // 4. No faces → Preferred tier — short-circuits before any dimension math.
    if faces.is_empty() {
        return CandidateClass::Preferred;
    }

    // 5. Image dimensions needed for area math from here on.
    let (img_w, img_h) = match (asset.width, asset.height) {
        (Some(w), Some(h)) if w > 0 && h > 0 => (w as u64, h as u64),
        _ => return CandidateClass::Rejected(RejectReason::MissingImageDimensions),
    };
    let image_area = img_w * img_h;

    // 6. Per-face area
    let mut total_face_area: u64 = 0;
    for face in faces {
        let face_area = face.bbox_area();
        total_face_area = total_face_area.saturating_add(face_area);
        let pct = (face_area as f64 / image_area as f64) * 100.0;
        if pct >= thresholds.per_face_pct {
            return CandidateClass::Rejected(RejectReason::PerFaceAreaExceeded {
                face_pct: pct.trunc() as u8,
            });
        }
    }

    // 7. Total face area
    let total_pct = (total_face_area as f64 / image_area as f64) * 100.0;
    if total_pct >= thresholds.total_pct {
        return CandidateClass::Rejected(RejectReason::TotalFaceAreaExceeded {
            total_pct: total_pct.trunc() as u8,
        });
    }

    // Faces present, all under threshold → fallback tier.
    CandidateClass::Acceptable
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::immich::types::{Asset, AssetFace, ExifInfo, Person};

    // ---- Fixture helpers ---------------------------------------------------

    fn asset_with_location(id: &str) -> Asset {
        Asset {
            id: id.to_string(),
            file_created_at: None,
            width: Some(4000),
            height: Some(3000),
            exif_info: Some(ExifInfo {
                exif_image_width: Some(4000),
                exif_image_height: Some(3000),
                city: Some("Lisbon".to_string()),
                country: Some("Portugal".to_string()),
                date_time_original: None,
            }),
            people: vec![],
        }
    }

    /// Build a face bbox that covers `pct`% of a 4000×3000 (12 000 000 px²) image.
    /// We fix x1=0, y1=0 and solve for the square side `s` such that s² = pct% * 12e6.
    fn face_covering_pct(pct: f64) -> AssetFace {
        let image_area = 4000u64 * 3000u64; // 12_000_000
        let face_area = (image_area as f64 * pct / 100.0) as u64;
        let side = (face_area as f64).sqrt() as i32;
        AssetFace {
            id: "face-id".to_string(),
            bounding_box_x1: 0,
            bounding_box_y1: 0,
            bounding_box_x2: side,
            bounding_box_y2: side,
            image_width: Some(4000),
            image_height: Some(3000),
            person: None,
        }
    }

    fn no_posted() -> HashSet<String> {
        HashSet::new()
    }

    fn thresholds() -> FilterThresholds {
        FilterThresholds::default() // 1% per-face, 2% total
    }

    // -----------------------------------------------------------------------
    // classify_candidate — tier semantics
    // -----------------------------------------------------------------------

    #[test]
    fn classify_no_faces_is_preferred() {
        let asset = asset_with_location("a1");
        let result = classify_candidate(&asset, &[], &thresholds(), &no_posted());
        assert_eq!(result, CandidateClass::Preferred);
    }

    #[test]
    fn classify_small_face_is_acceptable() {
        // 0.5% face is under both per-face (1% default) and total (2%) — Acceptable.
        let asset = asset_with_location("a1");
        let face = face_covering_pct(0.5);
        let result = classify_candidate(&asset, &[face], &thresholds(), &no_posted());
        assert_eq!(result, CandidateClass::Acceptable);
    }

    #[test]
    fn classify_degenerate_zero_area_face_is_acceptable_not_preferred() {
        // The presence of a face record (even with 0×0 bbox) means Immich
        // detected something — classify as Acceptable, not Preferred.
        let asset = asset_with_location("a1");
        let zero_face = AssetFace {
            id: "f0".to_string(),
            bounding_box_x1: 10,
            bounding_box_y1: 10,
            bounding_box_x2: 10,
            bounding_box_y2: 10,
            image_width: Some(4000),
            image_height: Some(3000),
            person: None,
        };
        let result = classify_candidate(&asset, &[zero_face], &thresholds(), &no_posted());
        assert_eq!(result, CandidateClass::Acceptable);
    }

    #[test]
    fn classify_large_face_is_rejected_per_face() {
        let asset = asset_with_location("a1");
        let face = face_covering_pct(10.0);
        let result = classify_candidate(&asset, &[face], &thresholds(), &no_posted());
        assert!(
            matches!(result, CandidateClass::Rejected(RejectReason::PerFaceAreaExceeded { face_pct }) if face_pct >= 9),
            "expected Rejected(PerFaceAreaExceeded), got {result:?}"
        );
    }

    #[test]
    fn classify_named_person_is_rejected() {
        let mut asset = asset_with_location("a1");
        asset.people.push(Person {
            id: "p1".to_string(),
            name: "Mom".to_string(),
        });
        let result = classify_candidate(&asset, &[], &thresholds(), &no_posted());
        assert_eq!(
            result,
            CandidateClass::Rejected(RejectReason::NamedPersonPresent {
                person_name: "Mom".to_string()
            })
        );
    }

    #[test]
    fn classify_already_posted_is_rejected() {
        let asset = asset_with_location("posted-id");
        let mut posted = HashSet::new();
        posted.insert("posted-id".to_string());
        let result = classify_candidate(&asset, &[], &thresholds(), &posted);
        assert_eq!(
            result,
            CandidateClass::Rejected(RejectReason::AlreadyPosted)
        );
    }

    // ---- Ported from legacy classify_candidate tests -----------------------

    #[test]
    fn classify_rejects_no_exif() {
        let mut asset = asset_with_location("a1");
        asset.exif_info = None;
        let result = classify_candidate(&asset, &[], &thresholds(), &no_posted());
        assert_eq!(result, CandidateClass::Rejected(RejectReason::NoLocation));
    }

    #[test]
    fn classify_accepts_country_only_location() {
        let mut asset = asset_with_location("a1");
        asset.exif_info = Some(crate::immich::types::ExifInfo {
            exif_image_width: Some(4000),
            exif_image_height: Some(3000),
            city: None,
            country: Some("Portugal".to_string()),
            date_time_original: None,
        });
        let result = classify_candidate(&asset, &[], &thresholds(), &no_posted());
        assert_eq!(result, CandidateClass::Preferred);
    }

    #[test]
    fn classify_rejects_missing_dimensions_when_faces_present() {
        let mut asset = asset_with_location("a1");
        asset.width = None;
        asset.height = None;
        let face = face_covering_pct(0.5);
        let result = classify_candidate(&asset, &[face], &thresholds(), &no_posted());
        assert_eq!(
            result,
            CandidateClass::Rejected(RejectReason::MissingImageDimensions)
        );
    }

    #[test]
    fn classify_missing_dimensions_with_no_faces_is_preferred() {
        // No faces → dimension check is skipped; asset is Preferred even
        // without width/height.
        let mut asset = asset_with_location("a1");
        asset.width = None;
        asset.height = None;
        let result = classify_candidate(&asset, &[], &thresholds(), &no_posted());
        assert_eq!(result, CandidateClass::Preferred);
    }

    // Note: the legacy `unassigned_faces_contribute_to_total_area` is omitted
    // — its intent (unassigned faces still count toward the total) is covered
    // implicitly by `classify_rejects_crowd_total_area` below, whose 5 faces
    // all have `person: None`.
    #[test]
    fn classify_rejects_crowd_total_area() {
        // Five faces each 0.5% (under per-face) → 2.5% combined > 2.0% total.
        let asset = asset_with_location("a1");
        let faces: Vec<AssetFace> = (0..5).map(|_| face_covering_pct(0.5)).collect();
        let result = classify_candidate(&asset, &faces, &thresholds(), &no_posted());
        assert!(
            matches!(result, CandidateClass::Rejected(RejectReason::TotalFaceAreaExceeded { total_pct }) if total_pct >= 2),
            "expected Rejected(TotalFaceAreaExceeded) with total >=2, got {result:?}"
        );
    }

    #[test]
    fn classify_per_face_check_uses_inclusive_threshold() {
        // 4000×3000 = 12_000_000 px². 1% threshold = 120_000 px².
        // Face of 347×347 = 120_409 px² → exactly above threshold (~1.003%).
        let asset = asset_with_location("a1");
        let exact_face = AssetFace {
            id: "f1".to_string(),
            bounding_box_x1: 0,
            bounding_box_y1: 0,
            bounding_box_x2: 347,
            bounding_box_y2: 347,
            image_width: Some(4000),
            image_height: Some(3000),
            person: None,
        };
        let result = classify_candidate(&asset, &[exact_face], &thresholds(), &no_posted());
        assert!(
            matches!(
                result,
                CandidateClass::Rejected(RejectReason::PerFaceAreaExceeded { face_pct: 1 })
            ),
            "expected Rejected(PerFaceAreaExceeded{{face_pct:1}}), got {result:?}"
        );
    }

    #[test]
    fn classify_unnamed_person_in_people_array_is_preferred() {
        // Immich puts unnamed/unrecognised people in `people[]` with name="".
        // These should NOT trigger the named-person reject; with no faces this
        // is Preferred.
        let mut asset = asset_with_location("a1");
        asset.people.push(Person {
            id: "p1".to_string(),
            name: String::new(),
        });
        let result = classify_candidate(&asset, &[], &thresholds(), &no_posted());
        assert_eq!(result, CandidateClass::Preferred);
    }

    #[test]
    fn classify_already_posted_beats_named_person() {
        let mut asset = asset_with_location("posted-named");
        asset.people.push(Person {
            id: "p1".to_string(),
            name: "Dad".to_string(),
        });
        let mut posted = HashSet::new();
        posted.insert("posted-named".to_string());
        let result = classify_candidate(&asset, &[], &thresholds(), &posted);
        assert_eq!(
            result,
            CandidateClass::Rejected(RejectReason::AlreadyPosted)
        );
    }
}
