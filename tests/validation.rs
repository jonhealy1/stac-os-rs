// `ENABLE_STAC_VALIDATOR=true` regression coverage: items are schema-
// validated in read shape (inferred links/`collection`/`assets` are
// API-generated and absent from stored docs), unknown `stac_version`
// values are rejected before the validator can panic or phone home,
// bulk members get per-item errors, and `If-Match` preconditions are
// checked before schema validation.
mod common;

use axum::http::StatusCode;
use common::*;
use serde_json::{json, Value};

/// `item()` plus the members the schema requires but the fixture omits.
fn valid_item(id: &str, collection_id: &str) -> Value {
    let mut i = item(id, collection_id);
    i["assets"] = json!({});
    i
}

async fn post_collection_and_item(
    app: &axum::Router,
    col: &str,
    item_id: &str,
) -> (StatusCode, Value) {
    let (s, _) = call(app, "POST", "/collections", Some(collection(col))).await;
    assert_eq!(s, StatusCode::CREATED);
    call(
        app,
        "POST",
        &format!("/collections/{col}/items"),
        Some(valid_item(item_id, col)),
    )
    .await
}

/// Regression: stored docs strip inferred rels, so the merged PATCH doc
/// never carries `rel=collection` — validating it raw 400s every PATCH.
/// Validation must run on the read-shape doc.
#[tokio::test]
async fn test_patch_item_validates_read_shape() {
    let Some((app, _)) = test_app_validated().await else {
        return;
    };
    let (col, id) = (uniq("vp-col"), uniq("vp-item"));
    let (s, _) = post_collection_and_item(&app, &col, &id).await;
    assert_eq!(s, StatusCode::CREATED);

    let (s, body) = call(
        &app,
        "PATCH",
        &format!("/collections/{col}/items/{id}"),
        Some(json!({"properties": {"datetime": "2021-01-01T00:00:00Z"}})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["properties"]["datetime"], "2021-01-01T00:00:00Z");
}

/// A patch that produces a schema-invalid merge is still a 400.
#[tokio::test]
async fn test_patch_item_rejects_invalid_merge() {
    let Some((app, _)) = test_app_validated().await else {
        return;
    };
    let (col, id) = (uniq("vp-col"), uniq("vp-item"));
    let (s, _) = post_collection_and_item(&app, &col, &id).await;
    assert_eq!(s, StatusCode::CREATED);

    let (s, body) = call(
        &app,
        "PATCH",
        &format!("/collections/{col}/items/{id}"),
        Some(json!({"bbox": "not-an-array"})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(
        body.to_string().contains("STAC validation failed"),
        "{body}"
    );
}

/// POST and PUT bodies where `collection` is set but no `rel=collection`
/// link is present are accepted — the API generates that link itself.
/// (The item schema's else-branch forbids `collection` without it, which
/// would otherwise 400 every minimal write.)
#[tokio::test]
async fn test_item_writes_collection_without_link() {
    let Some((app, _)) = test_app_validated().await else {
        return;
    };
    let (col, id) = (uniq("vp-col"), uniq("vp-item"));
    let (s, _) = call(&app, "POST", "/collections", Some(collection(&col))).await;
    assert_eq!(s, StatusCode::CREATED);

    let mut post = valid_item(&id, &col);
    post["links"] = json!([]);
    let (s, body) = call(
        &app,
        "POST",
        &format!("/collections/{col}/items"),
        Some(post),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{body}");

    // Same shape on PUT.
    let (s, body) = call(
        &app,
        "PUT",
        &format!("/collections/{col}/items/{id}"),
        Some(json!({
            "type": "Feature",
            "id": id,
            "stac_version": "1.0.0",
            "geometry": {"type": "Point", "coordinates": [1.0, 2.0]},
            "bbox": [0.0, 1.0, 2.0, 3.0],
            "properties": {"datetime": "2020-02-12T12:30:22Z"},
            "collection": col,
            "links": [],
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
}

/// Unknown `stac_version` values are rejected with 400 — the validator
/// builds a schema URI from the raw string and would otherwise panic on
/// URI-invalid chars or fetch schemas.stacspec.org for plausible ones.
#[tokio::test]
async fn test_item_writes_reject_unknown_stac_version() {
    let Some((app, _)) = test_app_validated().await else {
        return;
    };
    let (col, id) = (uniq("vp-col"), uniq("vp-item"));
    let (s, _) = post_collection_and_item(&app, &col, &id).await;
    assert_eq!(s, StatusCode::CREATED);

    for version in ["a b", "2.0.0"] {
        let mut bad = valid_item(&uniq("vp-bad"), &col);
        bad["stac_version"] = json!(version);
        let (s, body) = call(
            &app,
            "POST",
            &format!("/collections/{col}/items"),
            Some(bad),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "stac_version={version}: {body}");

        let (s, body) = call(
            &app,
            "PATCH",
            &format!("/collections/{col}/items/{id}"),
            Some(json!({"stac_version": version})),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "stac_version={version}: {body}");
    }
}

/// FeatureCollection batch members are validated individually; the
/// batch stays all-or-nothing on the first failure.
#[tokio::test]
async fn test_feature_collection_batch_validates_members() {
    let Some((app, _)) = test_app_validated().await else {
        return;
    };
    let col = uniq("vp-col");
    let (s, _) = call(&app, "POST", "/collections", Some(collection(&col))).await;
    assert_eq!(s, StatusCode::CREATED);
    let (a, b) = (uniq("vp-a"), uniq("vp-b"));

    let (s, body) = call(
        &app,
        "POST",
        &format!("/collections/{col}/items"),
        Some(json!({
            "type": "FeatureCollection",
            "features": [valid_item(&a, &col), valid_item(&b, &col)],
        })),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{body}");

    // A member missing `type` fails schema validation — whole batch 400s.
    let mut bad_member = valid_item(&uniq("vp-bad"), &col);
    bad_member.as_object_mut().unwrap().remove("type");
    let (s, _) = call(
        &app,
        "POST",
        &format!("/collections/{col}/items"),
        Some(json!({
            "type": "FeatureCollection",
            "features": [valid_item(&uniq("vp-c"), &col), bad_member],
        })),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

/// `bulk_items` schema errors land in `errors[]` like other malformed
/// admissions instead of rejecting the whole payload.
#[tokio::test]
async fn test_bulk_items_validates_members() {
    let Some((app, _)) = test_app_validated().await else {
        return;
    };
    let col = uniq("vp-col");
    let (s, _) = call(&app, "POST", "/collections", Some(collection(&col))).await;
    assert_eq!(s, StatusCode::CREATED);
    let good = uniq("vp-good");

    let mut no_type = valid_item(&uniq("vp-bad"), &col);
    no_type.as_object_mut().unwrap().remove("type");
    let (s, body) = call(
        &app,
        "POST",
        &format!("/collections/{col}/bulk_items"),
        Some(json!({"items": {good.clone(): valid_item(&good, &col), "bad": no_type}})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["success"], 1, "{body}");
    assert_eq!(body["received"], 2);
    let errors = body["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1, "{body}");
    assert!(
        errors[0]["msg"]
            .as_str()
            .unwrap_or_default()
            .contains("STAC validation failed"),
        "{errors:?}"
    );
}

/// With `REQUIRE_IF_MATCH` on, the 428 precondition fires before patch
/// application and schema validation — a garbage patch still gets 428.
#[tokio::test]
async fn test_patch_require_if_match_precedes_validation() {
    let Some((app, _)) = test_app_validated_if_match().await else {
        return;
    };
    let (col, id) = (uniq("vp-col"), uniq("vp-item"));
    let (s, _) = post_collection_and_item(&app, &col, &id).await;
    assert_eq!(s, StatusCode::CREATED);

    let (s, body) = call(
        &app,
        "PATCH",
        &format!("/collections/{col}/items/{id}"),
        Some(json!({"bbox": "not-an-array"})),
    )
    .await;
    assert_eq!(s, StatusCode::PRECONDITION_REQUIRED, "{body}");

    // A valid If-Match then flows through patch + validation normally.
    let (_, headers, _) =
        call_headers(&app, "GET", &format!("/collections/{col}/items/{id}"), None).await;
    let etag = headers.get("etag").unwrap().to_str().unwrap().to_string();
    let req = axum::http::Request::builder()
        .method("PATCH")
        .uri(format!("/collections/{col}/items/{id}"))
        .header("content-type", "application/json")
        .header("if-match", etag)
        .body(axum::body::Body::from(
            json!({"properties": {"datetime": "2022-02-02T00:00:00Z"}}).to_string(),
        ))
        .unwrap();
    let resp = tower::ServiceExt::oneshot(app, req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

/// A non-object write body is a 400, never a panic in link injection.
#[tokio::test]
async fn test_post_item_non_object_body() {
    let Some((app, _)) = test_app_validated().await else {
        return;
    };
    let col = uniq("vp-col");
    let (s, _) = call(&app, "POST", "/collections", Some(collection(&col))).await;
    assert_eq!(s, StatusCode::CREATED);

    let (s, body) = call(
        &app,
        "POST",
        &format!("/collections/{col}/items"),
        Some(json!(42)),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{body}");
}

/// Collection PATCH under validation is unaffected by the item link
/// invariant (stored collection docs keep `links` present).
#[tokio::test]
async fn test_patch_collection_validates() {
    let Some((app, _)) = test_app_validated().await else {
        return;
    };
    let col = uniq("vp-col");
    let (s, _) = call(&app, "POST", "/collections", Some(collection(&col))).await;
    assert_eq!(s, StatusCode::CREATED);

    let (s, body) = call(
        &app,
        "PATCH",
        &format!("/collections/{col}"),
        Some(json!({"title": "Renamed"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["title"], "Renamed");
}
