// tests/collections.rs — root /collections* read routes (core STAC).
mod common;

use axum::http::StatusCode;
use common::*;
use serde_json::json;

#[tokio::test]
async fn test_list_collections() {
    let Some((app, _)) = test_app(true).await else {
        return;
    };
    let cat = uniq("cl-cat");
    call(&app, "POST", "/catalogs", Some(catalog(&cat))).await;
    for i in 0..3 {
        call(
            &app,
            "POST",
            &format!("/catalogs/{cat}/collections"),
            Some(collection(&uniq(&format!("cl-col-{i}")))),
        )
        .await;
    }
    let (s, body) = call(&app, "GET", "/collections", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["numberMatched"], 3);
    assert_eq!(body["collections"].as_array().unwrap().len(), 3);
    for col in body["collections"].as_array().unwrap() {
        let rels = link_rels(col);
        for rel in ["self", "root", "parent"] {
            assert!(rels.contains(&rel.to_string()), "missing {rel}");
        }
        // canonical self link — the global route, not a scoped path
        let selfs = links_by_rel(col, "self");
        assert!(selfs[0]["href"]
            .as_str()
            .unwrap()
            .ends_with(&format!("/collections/{}", col["id"].as_str().unwrap())));
    }
}

#[tokio::test]
async fn test_list_collections_pagination() {
    let Some((app, _)) = test_app(true).await else {
        return;
    };
    let cat = uniq("clp-cat");
    call(&app, "POST", "/catalogs", Some(catalog(&cat))).await;
    for i in 0..3 {
        call(
            &app,
            "POST",
            &format!("/catalogs/{cat}/collections"),
            Some(collection(&uniq(&format!("clp-col-{i}")))),
        )
        .await;
    }
    let (s, page1) = call(&app, "GET", "/collections?limit=2", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(page1["collections"].as_array().unwrap().len(), 2);
    assert_eq!(page1["numberMatched"], 3);
    let next = links_by_rel(&page1, "next")
        .pop()
        .expect("missing next link");
    let token = next["href"]
        .as_str()
        .unwrap()
        .split("token=")
        .nth(1)
        .unwrap()
        .split('&')
        .next()
        .unwrap();
    let (s, page2) = call(
        &app,
        "GET",
        &format!("/collections?limit=2&token={token}"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(page2["collections"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn test_get_collection_global_links() {
    // Poly-hierarchy collection read via canonical route: parent link per
    // parent catalog, duplicate links to each scoped path.
    let Some((app, _)) = test_app(true).await else {
        return;
    };
    let (p1, p2) = (uniq("gl-p1"), uniq("gl-p2"));
    let col = uniq("gl-col");
    for p in [&p1, &p2] {
        call(&app, "POST", "/catalogs", Some(catalog(p))).await;
    }
    call(
        &app,
        "POST",
        &format!("/catalogs/{p1}/collections"),
        Some(collection(&col)),
    )
    .await;
    call(
        &app,
        "POST",
        &format!("/catalogs/{p2}/collections"),
        Some(json!({"id": col})),
    )
    .await;

    let (s, body) = call(&app, "GET", &format!("/collections/{col}"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["id"], col);
    let selfs = links_by_rel(&body, "self");
    assert!(selfs[0]["href"]
        .as_str()
        .unwrap()
        .ends_with(&format!("/collections/{col}")));
    let parents = links_by_rel(&body, "parent");
    let hrefs: Vec<&str> = parents.iter().filter_map(|l| l["href"].as_str()).collect();
    assert!(hrefs
        .iter()
        .any(|h| h.ends_with(&format!("/catalogs/{p1}"))));
    assert!(hrefs
        .iter()
        .any(|h| h.ends_with(&format!("/catalogs/{p2}"))));
    let dupes = links_by_rel(&body, "duplicate");
    assert_eq!(dupes.len(), 2);
}

#[tokio::test]
async fn test_get_collection_not_found() {
    let Some((app, _)) = test_app(true).await else {
        return;
    };
    let (s, _) = call(&app, "GET", "/collections/nonexistent", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_collection_items_global_route() {
    let Some((app, _)) = test_app(true).await else {
        return;
    };
    let cat = uniq("ci-cat");
    let col = uniq("ci-col");
    let it = uniq("ci-item");
    call(&app, "POST", "/catalogs", Some(catalog(&cat))).await;
    call(
        &app,
        "POST",
        &format!("/catalogs/{cat}/collections"),
        Some(collection(&col)),
    )
    .await;
    let (s, _) = call(
        &app,
        "POST",
        &format!("/catalogs/{cat}/collections/{col}/items"),
        Some(item(&it, &col)),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);

    let (s, body) = call(&app, "GET", &format!("/collections/{col}/items"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["type"], "FeatureCollection");
    assert_eq!(body["numberMatched"], 1);
    assert_eq!(body["features"][0]["id"], it);

    let (s, body) = call(&app, "GET", &format!("/collections/{col}/items/{it}"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["id"], it);
}

#[tokio::test]
async fn test_collection_items_404s() {
    let Some((app, _)) = test_app(true).await else {
        return;
    };
    let (s, _) = call(&app, "GET", "/collections/nope/items", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = call(&app, "GET", "/collections/nope/items/x", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_collection_items_pagination() {
    let Some((app, _)) = test_app(true).await else {
        return;
    };
    let cat = uniq("ip-cat");
    let col = uniq("ip-col");
    call(&app, "POST", "/catalogs", Some(catalog(&cat))).await;
    call(
        &app,
        "POST",
        &format!("/catalogs/{cat}/collections"),
        Some(collection(&col)),
    )
    .await;
    for i in 0..3 {
        call(
            &app,
            "POST",
            &format!("/catalogs/{cat}/collections/{col}/items"),
            Some(item(&uniq(&format!("ip-item-{i}")), &col)),
        )
        .await;
    }
    let (s, page1) = call(
        &app,
        "GET",
        &format!("/collections/{col}/items?limit=2"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(page1["numberReturned"], 2);
    assert_eq!(page1["numberMatched"], 3);
    let next = links_by_rel(&page1, "next")
        .pop()
        .expect("missing next link");
    let href = next["href"].as_str().unwrap();
    assert!(href.contains("token="));
    let token = href
        .split("token=")
        .nth(1)
        .unwrap()
        .split('&')
        .next()
        .unwrap();
    let (s, page2) = call(
        &app,
        "GET",
        &format!("/collections/{col}/items?limit=2&token={token}"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(page2["numberReturned"], 1);
}

#[tokio::test]
async fn test_scoped_items_pagination() {
    let Some((app, _)) = test_app(true).await else {
        return;
    };
    let cat = uniq("sip-cat");
    let col = uniq("sip-col");
    call(&app, "POST", "/catalogs", Some(catalog(&cat))).await;
    call(
        &app,
        "POST",
        &format!("/catalogs/{cat}/collections"),
        Some(collection(&col)),
    )
    .await;
    for i in 0..3 {
        call(
            &app,
            "POST",
            &format!("/catalogs/{cat}/collections/{col}/items"),
            Some(item(&uniq(&format!("sip-item-{i}")), &col)),
        )
        .await;
    }
    let (s, page1) = call(
        &app,
        "GET",
        &format!("/catalogs/{cat}/collections/{col}/items?limit=2"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(page1["numberReturned"], 2);
    assert_eq!(page1["numberMatched"], 3);
}

#[tokio::test]
async fn test_sortables_endpoint() {
    let Some((app, _)) = test_app(true).await else {
        return;
    };
    let (s, body) = call(&app, "GET", "/sortables", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["type"], "object");
    assert_eq!(body["additionalProperties"], true);
    let props = body["properties"].as_object().unwrap();
    assert!(props.contains_key("id"));
    assert!(props.contains_key("collection"));
    assert!(props.contains_key("properties.datetime"));
}

#[tokio::test]
async fn test_landing_advertises_sort_and_sortables() {
    let Some((app, _)) = test_app(true).await else {
        return;
    };
    let (s, body) = call(&app, "GET", "/", None).await;
    assert_eq!(s, StatusCode::OK);
    let conforms: Vec<&str> = body["conformsTo"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|u| u.as_str())
        .collect();
    for uri in [
        "https://api.stacspec.org/v1.0.0/item-search",
        "https://api.stacspec.org/v1.1.0/item-search#sort",
        "https://api.stacspec.org/v1.1.0/item-search#sortables",
    ] {
        assert!(conforms.contains(&uri), "missing {uri}");
    }
    let sortables = links_by_rel(&body, "http://www.opengis.net/def/rel/ogc/1.0/sortables");
    assert_eq!(sortables.len(), 1);
    assert!(sortables[0]["href"]
        .as_str()
        .unwrap()
        .ends_with("/sortables"));
}

// --- Core transaction routes (canonical /collections surface) ---

#[tokio::test]
async fn test_core_collection_crud() {
    let Some((app, _)) = test_app(true).await else {
        return;
    };
    let col = uniq("core-col");
    // POST /collections creates under root
    let (s, body) = call(&app, "POST", "/collections", Some(collection(&col))).await;
    assert_eq!(s, StatusCode::CREATED);
    assert!(body["links"]
        .as_array()
        .unwrap()
        .iter()
        .any(|l| l["rel"] == "self"));

    // repost -> 409
    let (s, _) = call(&app, "POST", "/collections", Some(collection(&col))).await;
    assert_eq!(s, StatusCode::CONFLICT);

    // PUT update
    let mut updated = collection(&col);
    updated["title"] = json!("Renamed");
    let (s, body) = call(&app, "PUT", &format!("/collections/{col}"), Some(updated)).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["title"], "Renamed");

    // PUT id mismatch -> 400; missing -> 404
    let (s, _) = call(
        &app,
        "PUT",
        &format!("/collections/{col}"),
        Some(collection("different-id")),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = call(&app, "PUT", "/collections/nope", Some(collection("nope"))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // DELETE -> 204, then 404
    let (s, _) = call(&app, "DELETE", &format!("/collections/{col}"), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = call(&app, "GET", &format!("/collections/{col}"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_core_item_crud() {
    let Some((app, _)) = test_app(true).await else {
        return;
    };
    let col = uniq("ci-col");
    let it = uniq("ci-item");
    call(&app, "POST", "/collections", Some(collection(&col))).await;

    // item create -> 201 with links + geo+json content type
    let (s, body) = call(
        &app,
        "POST",
        &format!("/collections/{col}/items"),
        Some(item(&it, &col)),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    assert!(body["links"]
        .as_array()
        .unwrap()
        .iter()
        .any(|l| l["rel"] == "self"));

    // repost same item id -> 409
    let (s, _) = call(
        &app,
        "POST",
        &format!("/collections/{col}/items"),
        Some(item(&it, &col)),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);

    // GET via canonical route
    let (s, body) = call(&app, "GET", &format!("/collections/{col}/items/{it}"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["id"], it);

    // PUT update
    let mut upd = item(&it, &col);
    upd["properties"]["datetime"] = json!("2024-01-01T00:00:00Z");
    let (s, _) = call(
        &app,
        "PUT",
        &format!("/collections/{col}/items/{it}"),
        Some(upd),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    // PUT missing item -> 404; item in wrong collection -> 404
    let (s, _) = call(
        &app,
        "PUT",
        &format!("/collections/{col}/items/ghost"),
        Some(item("ghost", &col)),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // POST item to missing collection -> 404
    let (s, _) = call(
        &app,
        "POST",
        "/collections/nope/items",
        Some(item(&uniq("orph"), "nope")),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // DELETE -> 204 then 404
    let (s, _) = call(
        &app,
        "DELETE",
        &format!("/collections/{col}/items/{it}"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = call(&app, "GET", &format!("/collections/{col}/items/{it}"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_delete_collection_removes_items() {
    let Some((app, _)) = test_app(true).await else {
        return;
    };
    let col = uniq("dc-col");
    let it = uniq("dc-item");
    call(&app, "POST", "/collections", Some(collection(&col))).await;
    call(
        &app,
        "POST",
        &format!("/collections/{col}/items"),
        Some(item(&it, &col)),
    )
    .await;

    let (s, _) = call(&app, "DELETE", &format!("/collections/{col}"), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);

    // Collection deletion cascades to its items
    let (s, body) = call(&app, "POST", "/search", Some(json!({"ids": [it]}))).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["numberMatched"], 0);
}

#[tokio::test]
async fn test_items_sortby() {
    // Features-binding sort extension: ?sortby=+field/-field on items listing.
    let Some((app, _)) = test_app(true).await else {
        return;
    };
    let col = uniq("is-col");
    call(&app, "POST", "/collections", Some(collection(&col))).await;
    for (id, dt) in [
        ("a", "2023-01-03T00:00:00Z"),
        ("b", "2023-01-01T00:00:00Z"),
        ("c", "2023-01-02T00:00:00Z"),
    ] {
        let mut it = item(&uniq(&format!("is-{id}")), &col);
        it["properties"]["datetime"] = json!(dt);
        call(&app, "POST", &format!("/collections/{col}/items"), Some(it)).await;
    }

    let datetimes = |body: &serde_json::Value| -> Vec<String> {
        body["features"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|f| f["properties"]["datetime"].as_str().map(str::to_owned))
            .collect()
    };

    // asc shorthand: ?sortby=+properties.datetime (encoded %2B)
    let (_, asc) = call(
        &app,
        "GET",
        &format!("/collections/{col}/items?sortby=%2Bproperties.datetime"),
        None,
    )
    .await;
    let asc_dts = datetimes(&asc);
    let mut sorted = asc_dts.clone();
    sorted.sort();
    assert_eq!(asc_dts, sorted, "asc sort ordering failed");

    // desc shorthand
    let (_, desc) = call(
        &app,
        "GET",
        &format!("/collections/{col}/items?sortby=-properties.datetime"),
        None,
    )
    .await;
    let desc_dts = datetimes(&desc);
    sorted.reverse();
    assert_eq!(desc_dts, sorted, "desc sort ordering failed");
}

#[tokio::test]
async fn test_collection_sortables() {
    let Some((app, _)) = test_app(true).await else {
        return;
    };
    let col = uniq("cs-col");
    call(&app, "POST", "/collections", Some(collection(&col))).await;

    // endpoint exists + collection doc links to it
    let (s, body) = call(&app, "GET", &format!("/collections/{col}/sortables"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["type"], "object");
    // features binding is a closed schema (upstream `additional_properties=False`)
    assert_eq!(body["additionalProperties"], false);

    let (s, body) = call(&app, "GET", &format!("/collections/{col}"), None).await;
    assert_eq!(s, StatusCode::OK);
    let sortables = links_by_rel(&body, "http://www.opengis.net/def/rel/ogc/1.0/sortables");
    assert_eq!(sortables.len(), 1);
    assert!(sortables[0]["href"]
        .as_str()
        .unwrap()
        .ends_with(&format!("/collections/{col}/sortables")));

    // 404 for a missing collection
    let (s, _) = call(&app, "GET", "/collections/nope/sortables", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}
