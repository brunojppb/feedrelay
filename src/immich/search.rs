//! Immich smart-search (`POST /api/search/smart`) and face-fetch
//! (`GET /api/faces`) request functions.
//!
//! ## Response shape (verified against Immich source)
//!
//! `POST /api/search/smart` returns:
//! ```json
//! {
//!   "assets": {
//!     "total": 42,
//!     "count": 42,
//!     "items": [ /* Asset objects */ ],
//!     "facets": [],
//!     "nextPage": null
//!   },
//!   "albums": { ... }
//! }
//! ```
//!
//! Each `Asset` in `items` has top-level `width`/`height` pixel dimensions and
//! a `people[]` array of `PersonResponseDto` (id + name only — **no bounding
//! boxes**).  Bounding-box data lives in `GET /api/faces?id={asset_id}`.
//!
//! ## Face-area filtering
//!
//! Because bounding boxes are not part of the smart-search response, Task 5's
//! worker must call `fetch_faces` per asset (or per candidate subset) before
//! invoking `crate::filter::classify_candidate`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::immich::client::ImmichClient;
use crate::immich::types::{Asset, AssetFace};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors that can occur during Immich API calls.
#[derive(Debug, thiserror::Error)]
pub enum ImmichError {
    /// A `reqwest` transport or TLS error.
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),

    /// The server returned a non-2xx status.
    #[error("immich returned HTTP {status}: {body}")]
    Status { status: u16, body: String },

    /// Response body could not be deserialised.
    #[error("deserialize error: {0}")]
    Deserialize(serde_json::Error),
}

// ---------------------------------------------------------------------------
// search_smart
// ---------------------------------------------------------------------------

/// Parameters for a smart-search request.
#[derive(Debug, Clone)]
pub struct SmartSearchParams {
    /// CLIP natural-language query, e.g. `"landscape architecture nature scenery"`.
    pub query: String,
    /// Maximum number of results to return (Immich `size` field).
    pub size: u32,
    /// If set, restricts results to assets taken after this timestamp.
    pub taken_after: Option<DateTime<Utc>>,
}

/// Request body sent to `POST /api/search/smart`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SmartSearchBody<'a> {
    query: &'a str,
    size: u32,
    with_exif: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    taken_after: Option<String>, // ISO-8601
}

/// Outer envelope returned by `POST /api/search/smart`.
#[derive(Debug, Deserialize)]
struct SearchResponse {
    assets: AssetPage,
}

/// Paged asset list inside the search envelope.
#[derive(Debug, Deserialize)]
struct AssetPage {
    items: Vec<Asset>,
}

/// Call `POST /api/search/smart` and return the inner `assets.items` slice.
///
/// # Errors
///
/// Returns [`ImmichError::Http`] for transport failures,
/// [`ImmichError::Status`] for non-2xx responses, and
/// [`ImmichError::Deserialize`] if the body cannot be parsed.
pub async fn search_smart(
    client: &ImmichClient,
    params: &SmartSearchParams,
) -> Result<Vec<Asset>, ImmichError> {
    let body = SmartSearchBody {
        query: &params.query,
        size: params.size,
        with_exif: true,
        taken_after: params.taken_after.map(|dt| dt.to_rfc3339()),
    };

    let response = client
        .http
        .post(client.url("/api/search/smart"))
        .header("x-api-key", &client.api_key)
        .json(&body)
        .send()
        .await?;

    let status = response.status();
    if !status.is_success() {
        let body_text = response
            .text()
            .await
            .unwrap_or_else(|e| format!("<body read error: {e}>"));
        return Err(ImmichError::Status {
            status: status.as_u16(),
            body: body_text,
        });
    }

    let text = response.text().await?;
    let parsed: SearchResponse = serde_json::from_str(&text).map_err(ImmichError::Deserialize)?;
    Ok(parsed.assets.items)
}

// ---------------------------------------------------------------------------
// fetch_faces
// ---------------------------------------------------------------------------

/// Call `GET /api/faces?id={asset_id}` and return all detected faces.
///
/// Despite the misleading parameter name in Immich's API, `id` here is the
/// **asset** UUID, not a face UUID — see the `getFaces` operation in the
/// Immich OpenAPI spec ("Retrieve faces for asset").
///
/// Each [`AssetFace`] includes bounding-box coordinates and a nullable
/// `person` field.  Task 5 uses this to populate the face list before
/// calling [`crate::filter::classify_candidate`].
///
/// # Errors
///
/// Same error variants as [`search_smart`].
pub async fn fetch_faces(
    client: &ImmichClient,
    asset_id: &str,
) -> Result<Vec<AssetFace>, ImmichError> {
    let response = client
        .http
        .get(client.url("/api/faces"))
        .header("x-api-key", &client.api_key)
        .query(&[("id", asset_id)])
        .send()
        .await?;

    let status = response.status();
    if !status.is_success() {
        let body_text = response
            .text()
            .await
            .unwrap_or_else(|e| format!("<body read error: {e}>"));
        return Err(ImmichError::Status {
            status: status.as_u16(),
            body: body_text,
        });
    }

    let text = response.text().await?;
    let faces: Vec<AssetFace> = serde_json::from_str(&text).map_err(ImmichError::Deserialize)?;
    Ok(faces)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Synthetic Immich smart-search response with 2 assets.
    fn mock_search_response() -> serde_json::Value {
        serde_json::json!({
            "albums": {
                "total": 0,
                "count": 0,
                "items": [],
                "facets": [],
                "nextPage": null
            },
            "assets": {
                "total": 2,
                "count": 2,
                "nextPage": null,
                "facets": [],
                "items": [
                    {
                        "id": "asset-uuid-1",
                        "type": "IMAGE",
                        "thumbhash": null,
                        "originalMimeType": "image/jpeg",
                        "localDateTime": "2024-06-15T10:30:00.000Z",
                        "duration": "0:00:00.00000",
                        "livePhotoVideoId": null,
                        "hasMetadata": true,
                        "width": 4032,
                        "height": 3024,
                        "createdAt": "2024-06-15T10:30:00.000Z",
                        "updatedAt": "2024-06-15T10:30:00.000Z",
                        "fileCreatedAt": "2024-06-15T08:30:00.000Z",
                        "fileModifiedAt": "2024-06-15T08:30:00.000Z",
                        "ownerId": "owner-uuid",
                        "libraryId": null,
                        "originalPath": "/photos/lisbon.jpg",
                        "originalFileName": "lisbon.jpg",
                        "isFavorite": false,
                        "isArchived": false,
                        "isTrashed": false,
                        "isOffline": false,
                        "visibility": "public",
                        "checksum": "abc123",
                        "isEdited": false,
                        "exifInfo": {
                            "exifImageWidth": 4032,
                            "exifImageHeight": 3024,
                            "city": "Lisbon",
                            "country": "Portugal",
                            "dateTimeOriginal": "2024-06-15T08:30:00.000Z"
                        },
                        "people": [],
                        "tags": []
                    },
                    {
                        "id": "asset-uuid-2",
                        "type": "IMAGE",
                        "thumbhash": null,
                        "originalMimeType": "image/jpeg",
                        "localDateTime": "2024-08-20T14:00:00.000Z",
                        "duration": "0:00:00.00000",
                        "livePhotoVideoId": null,
                        "hasMetadata": true,
                        "width": 5472,
                        "height": 3648,
                        "createdAt": "2024-08-20T14:00:00.000Z",
                        "updatedAt": "2024-08-20T14:00:00.000Z",
                        "fileCreatedAt": "2024-08-20T12:00:00.000Z",
                        "fileModifiedAt": "2024-08-20T12:00:00.000Z",
                        "ownerId": "owner-uuid",
                        "libraryId": null,
                        "originalPath": "/photos/porto.jpg",
                        "originalFileName": "porto.jpg",
                        "isFavorite": true,
                        "isArchived": false,
                        "isTrashed": false,
                        "isOffline": false,
                        "visibility": "public",
                        "checksum": "def456",
                        "isEdited": false,
                        "exifInfo": {
                            "exifImageWidth": 5472,
                            "exifImageHeight": 3648,
                            "city": "Porto",
                            "country": "Portugal",
                            "dateTimeOriginal": "2024-08-20T12:00:00.000Z"
                        },
                        "people": [
                            { "id": "person-uuid-1", "name": "" }
                        ],
                        "tags": []
                    }
                ]
            }
        })
    }

    #[tokio::test]
    async fn search_smart_returns_two_assets_and_checks_request() {
        // Arrange
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/api/search/smart"))
            .and(header("x-api-key", "test-api-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(mock_search_response()))
            .expect(1)
            .mount(&server)
            .await;

        let client = ImmichClient::new(server.uri(), "test-api-key");
        let params = SmartSearchParams {
            query: "landscape architecture nature scenery".to_string(),
            size: 50,
            taken_after: Some("2024-01-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap()),
        };

        // Act
        let assets = search_smart(&client, &params)
            .await
            .expect("search_smart failed");

        // Assert: two assets returned
        assert_eq!(assets.len(), 2, "expected 2 assets");

        // First asset
        assert_eq!(assets[0].id, "asset-uuid-1");
        assert_eq!(assets[0].width, Some(4032));
        assert_eq!(assets[0].height, Some(3024));
        assert!(assets[0].people.is_empty());
        let exif0 = assets[0]
            .exif_info
            .as_ref()
            .expect("exif missing on asset 0");
        assert_eq!(exif0.city.as_deref(), Some("Lisbon"));
        assert_eq!(exif0.country.as_deref(), Some("Portugal"));

        // Second asset
        assert_eq!(assets[1].id, "asset-uuid-2");
        assert_eq!(assets[1].people.len(), 1);
        assert_eq!(assets[1].people[0].name, ""); // unnamed person

        // wiremock verifies the request was sent with the right headers
        // (the `.expect(1)` guard above ensures exactly one call was made)
    }

    #[tokio::test]
    async fn search_smart_propagates_non_2xx_as_status_error() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/api/search/smart"))
            .respond_with(ResponseTemplate::new(401).set_body_string("Unauthorized"))
            .mount(&server)
            .await;

        let client = ImmichClient::new(server.uri(), "bad-key");
        let params = SmartSearchParams {
            query: "test".to_string(),
            size: 10,
            taken_after: None,
        };

        let err = search_smart(&client, &params)
            .await
            .expect_err("expected an error for 401");
        assert!(
            matches!(err, ImmichError::Status { status: 401, .. }),
            "expected Status(401), got: {err:?}"
        );
    }

    /// Synthetic `GET /api/faces` response: two faces, one with a person and one without.
    fn mock_faces_response() -> serde_json::Value {
        serde_json::json!([
            {
                "id": "face-uuid-1",
                "boundingBoxX1": 100,
                "boundingBoxY1": 50,
                "boundingBoxX2": 300,
                "boundingBoxY2": 400,
                "imageWidth": 4032,
                "imageHeight": 3024,
                "person": { "id": "person-uuid-1", "name": "" }
            },
            {
                "id": "face-uuid-2",
                "boundingBoxX1": 500,
                "boundingBoxY1": 200,
                "boundingBoxX2": 800,
                "boundingBoxY2": 600,
                "imageWidth": 4032,
                "imageHeight": 3024,
                "person": null
            }
        ])
    }

    #[tokio::test]
    async fn fetch_faces_returns_face_array_and_checks_request() {
        // Arrange
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/faces"))
            .and(query_param("id", "asset-uuid-1"))
            .and(header("x-api-key", "test-api-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(mock_faces_response()))
            .expect(1)
            .mount(&server)
            .await;

        let client = ImmichClient::new(server.uri(), "test-api-key");

        // Act
        let faces = fetch_faces(&client, "asset-uuid-1")
            .await
            .expect("fetch_faces failed");

        // Assert: two faces returned
        assert_eq!(faces.len(), 2, "expected 2 faces");

        // First face has a person, second does not
        assert!(
            faces[0].person.is_some(),
            "expected first face to have a person"
        );
        assert!(
            faces[1].person.is_none(),
            "expected second face to have no person"
        );

        // Spot-check a bbox field to confirm deserialisation worked
        assert!(faces[0].bounding_box_x2 > 0, "expected bounding_box_x2 > 0");
        assert_eq!(faces[0].bounding_box_x2, 300);
        assert_eq!(faces[1].bounding_box_y2, 600);

        // wiremock's .expect(1) above enforces exactly one call was made
    }

    #[tokio::test]
    async fn fetch_faces_propagates_non_2xx_as_status_error() {
        // Arrange
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/faces"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;

        let client = ImmichClient::new(server.uri(), "test-api-key");

        // Act
        let err = fetch_faces(&client, "asset-uuid-1")
            .await
            .expect_err("expected an error for 403");

        // Assert
        assert!(
            matches!(err, ImmichError::Status { status: 403, .. }),
            "expected Status(403), got: {err:?}"
        );
    }
}
