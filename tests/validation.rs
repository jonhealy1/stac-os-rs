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

/// FeatureCollection batch members are validated individually;
/// non-strict mode inserts the valid members and reports the failures
/// in `errors` — upstream `bulk_item_insert` partial semantics.
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

    // A member missing `type` fails schema validation — it lands in
    // `errors` while the valid member is still written.
    let bad_id = uniq("vp-bad");
    let good_id = uniq("vp-c");
    let mut bad_member = valid_item(&bad_id, &col);
    bad_member.as_object_mut().unwrap().remove("type");
    let (s, body) = call(
        &app,
        "POST",
        &format!("/collections/{col}/items"),
        Some(json!({
            "type": "FeatureCollection",
            "features": [valid_item(&good_id, &col), bad_member],
        })),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    let errors = body["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1, "{body}");
    assert_eq!(errors[0]["id"], bad_id, "{body}");
    assert!(
        errors[0]["msg"]
            .as_str()
            .unwrap_or_default()
            .contains("STAC validation failed"),
        "{errors:?}"
    );
    let (s, _) = call(
        &app,
        "GET",
        &format!("/collections/{col}/items/{good_id}"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
}

/// Strict mode (`RAISE_ON_BULK_ERROR=true`): one invalid member
/// rejects the whole batch with 400.
#[tokio::test]
async fn test_feature_collection_batch_strict_rejects() {
    let Some((app, _)) = test_app_flags(TestFlags {
        enable_validator: true,
        raise_on_bulk_error: true,
        ..Default::default()
    })
    .await
    else {
        return;
    };
    let col = uniq("vps-col");
    let (s, _) = call(&app, "POST", "/collections", Some(collection(&col))).await;
    assert_eq!(s, StatusCode::CREATED);

    let good_id = uniq("vps-good");
    let mut bad_member = valid_item(&uniq("vps-bad"), &col);
    bad_member.as_object_mut().unwrap().remove("type");
    let (s, body) = call(
        &app,
        "POST",
        &format!("/collections/{col}/items"),
        Some(json!({
            "type": "FeatureCollection",
            "features": [valid_item(&good_id, &col), bad_member],
        })),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("Batch rejected"), "{body}");
    let (s, _) = call(
        &app,
        "GET",
        &format!("/collections/{col}/items/{good_id}"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
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

/// Single-item schema failures carry upstream's message:
/// `Invalid item: STAC validation failed for '{id}': ...`.
#[tokio::test]
async fn test_item_write_error_message_format() {
    let Some((app, _)) = test_app_validated().await else {
        return;
    };
    let col = uniq("vp-col");
    let (s, _) = call(&app, "POST", "/collections", Some(collection(&col))).await;
    assert_eq!(s, StatusCode::CREATED);

    let id = uniq("vp-msg");
    let mut bad = valid_item(&id, &col);
    bad.as_object_mut().unwrap().remove("type");
    let (s, body) = call(
        &app,
        "POST",
        &format!("/collections/{col}/items"),
        Some(bad),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body.to_string()
            .contains(&format!("Invalid item: STAC validation failed for '{id}'")),
        "{body}"
    );
}

/// Upstream `validate_datetime_range` — gated on the validator flag:
/// null datetime needs start+end, the bounds come as a pair, and
/// `start <= end` (and `datetime` inside them when present).
#[tokio::test]
async fn test_item_writes_validate_datetime_range() {
    let Some((app, _)) = test_app_validated().await else {
        return;
    };
    let col = uniq("vdt-col");
    let (s, _) = call(&app, "POST", "/collections", Some(collection(&col))).await;
    assert_eq!(s, StatusCode::CREATED);

    // null datetime without start/end → 400. The item schema itself
    // rejects this before the range check runs (upstream ordering is
    // identical — schema first, then `validate_datetime_range`).
    let mut bad = valid_item(&uniq("vdt-a"), &col);
    bad["properties"]["datetime"] = serde_json::Value::Null;
    let (s, body) = call(
        &app,
        "POST",
        &format!("/collections/{col}/items"),
        Some(bad),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{body}");

    // start without end → 400 (the schema's required-pair rule catches
    // it before the range check — same ordering as upstream).
    let mut bad = valid_item(&uniq("vdt-b"), &col);
    bad["properties"]["start_datetime"] = json!("2020-01-01T00:00:00Z");
    let (s, body) = call(
        &app,
        "POST",
        &format!("/collections/{col}/items"),
        Some(bad),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{body}");

    // start > end → 400
    let mut bad = valid_item(&uniq("vdt-c"), &col);
    bad["properties"]["start_datetime"] = json!("2021-01-01T00:00:00Z");
    bad["properties"]["end_datetime"] = json!("2020-01-01T00:00:00Z");
    let (s, body) = call(
        &app,
        "POST",
        &format!("/collections/{col}/items"),
        Some(bad),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("Invalid item datetime"), "{body}");

    // datetime outside [start, end] → 400
    let mut bad = valid_item(&uniq("vdt-d"), &col);
    bad["properties"]["datetime"] = json!("2022-01-01T00:00:00Z");
    bad["properties"]["start_datetime"] = json!("2020-01-01T00:00:00Z");
    bad["properties"]["end_datetime"] = json!("2021-01-01T00:00:00Z");
    let (s, body) = call(
        &app,
        "POST",
        &format!("/collections/{col}/items"),
        Some(bad),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("Invalid item datetime"), "{body}");

    // null datetime + valid range → accepted (upstream
    // `allows_null_datetime_with_start_and_end`)
    let mut ok = valid_item(&uniq("vdt-ok"), &col);
    ok["properties"]["datetime"] = serde_json::Value::Null;
    ok["properties"]["start_datetime"] = json!("2020-01-01T00:00:00Z");
    ok["properties"]["end_datetime"] = json!("2021-01-01T00:00:00Z");
    let (s, body) = call(&app, "POST", &format!("/collections/{col}/items"), Some(ok)).await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
}

/// `ENABLE_TOPOLOGY_VALIDATION` — WGS84 bounds, ring minimum, vertex
/// ceiling, antimeridian jumps; message prefix `Invalid item geometry`.
#[tokio::test]
async fn test_item_topology_validation() {
    let Some((app, _)) = test_app_flags(TestFlags {
        enable_topology_validation: true,
        max_topology_vertices: 8,
        ..Default::default()
    })
    .await
    else {
        return;
    };
    let col = uniq("vtp-col");
    let (s, _) = call(&app, "POST", "/collections", Some(collection(&col))).await;
    assert_eq!(s, StatusCode::CREATED);
    let post = |body: Value| {
        let col = col.clone();
        let app = app.clone();
        async move {
            call(
                &app,
                "POST",
                &format!("/collections/{col}/items"),
                Some(body),
            )
            .await
        }
    };

    // Longitude outside [-180, 180] → 400
    let mut bad = item(&uniq("vtp-bounds"), &col);
    bad["geometry"] = json!({"type": "Point", "coordinates": [190.0, 10.0]});
    let (s, body) = post(bad).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("Invalid item geometry"), "{body}");

    // Polygon ring crossing the antimeridian (>180° lon jump) → 400
    let mut bad = item(&uniq("vtp-anti"), &col);
    bad["geometry"] = json!({
        "type": "Polygon",
        "coordinates": [[[170.0, 10.0], [-170.0, 10.0], [-170.0, 5.0], [170.0, 5.0], [170.0, 10.0]]]
    });
    let (s, body) = post(bad).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("antimeridian"), "{body}");

    // Ring exceeding MAX_TOPOLOGY_VERTICES (8 here) → 400
    let mut bad = item(&uniq("vtp-verts"), &col);
    bad["geometry"] = json!({
        "type": "Polygon",
        "coordinates": [[[0.0,0.0],[1.0,0.0],[2.0,0.0],[2.0,1.0],[1.0,2.0],[0.0,2.0],[-1.0,1.0],[0.0,0.5],[0.0,0.0]]]
    });
    let (s, body) = post(bad).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("too many vertices"), "{body}");

    // A valid polygon within bounds and the vertex ceiling → 201
    let mut ok = item(&uniq("vtp-ok"), &col);
    ok["geometry"] = json!({
        "type": "Polygon",
        "coordinates": [[[0.0,0.0],[1.0,0.0],[1.0,1.0],[0.0,1.0],[0.0,0.0]]]
    });
    let (s, body) = post(ok).await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
}

/// `bulk_items` strict mode — one schema-invalid member rejects the
/// whole payload with `Bulk insertion rejected`.
#[tokio::test]
async fn test_bulk_items_raise_on_error_rejects() {
    let Some((app, _)) = test_app_flags(TestFlags {
        enable_validator: true,
        raise_on_bulk_error: true,
        ..Default::default()
    })
    .await
    else {
        return;
    };
    let col = uniq("vbr-col");
    let (s, _) = call(&app, "POST", "/collections", Some(collection(&col))).await;
    assert_eq!(s, StatusCode::CREATED);
    let good = uniq("vbr-good");
    let mut bad = valid_item(&uniq("vbr-bad"), &col);
    bad.as_object_mut().unwrap().remove("type");

    let (s, body) = call(
        &app,
        "POST",
        &format!("/collections/{col}/bulk_items"),
        Some(json!({"items": {good.clone(): valid_item(&good, &col), "bad": bad}})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body.to_string().contains("Bulk insertion rejected"),
        "{body}"
    );
    let (s, _) = call(
        &app,
        "GET",
        &format!("/collections/{col}/items/{good}"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

/// `MAX_BATCH_ERROR_SIZE` — exceeding the validation-error ceiling
/// rejects regardless of `raise_on_error`, with upstream's
/// threshold-exceeded text.
#[tokio::test]
async fn test_bulk_items_max_batch_error_size() {
    let Some((app, _)) = test_app_flags(TestFlags {
        enable_validator: true,
        max_batch_error_size: Some(1),
        ..Default::default()
    })
    .await
    else {
        return;
    };
    let col = uniq("vbx-col");
    let (s, _) = call(&app, "POST", "/collections", Some(collection(&col))).await;
    assert_eq!(s, StatusCode::CREATED);

    let mut bad1 = valid_item(&uniq("vbx-a"), &col);
    bad1.as_object_mut().unwrap().remove("type");
    let mut bad2 = valid_item(&uniq("vbx-b"), &col);
    bad2.as_object_mut().unwrap().remove("type");
    let (s, body) = call(
        &app,
        "POST",
        &format!("/collections/{col}/bulk_items"),
        Some(json!({"items": {"a": bad1, "b": bad2}})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body.to_string()
            .contains("Validation error threshold exceeded"),
        "{body}"
    );
}

/// Bulk upsert under `REQUIRE_IF_MATCH` 428s with upstream's
/// `use method 'insert'` guidance.
#[tokio::test]
async fn test_bulk_items_upsert_if_match_message() {
    let Some((app, _)) = test_app_flags(TestFlags {
        require_if_match: true,
        ..Default::default()
    })
    .await
    else {
        return;
    };
    let col = uniq("vup-col");
    let (s, _) = call(&app, "POST", "/collections", Some(collection(&col))).await;
    assert_eq!(s, StatusCode::CREATED);
    let id = uniq("vup-item");

    let (s, body) = call(
        &app,
        "POST",
        &format!("/collections/{col}/bulk_items"),
        Some(json!({"method": "upsert", "items": {id.clone(): valid_item(&id, &col)}})),
    )
    .await;
    assert_eq!(s, StatusCode::PRECONDITION_REQUIRED, "{body}");
    assert!(body.to_string().contains("method 'insert'"), "{body}");
}
