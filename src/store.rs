// src/store.rs
use opensearch::{
    http::{
        transport::{SingleNodeConnectionPool, TransportBuilder},
        StatusCode, Url,
    },
    indices::{IndicesCreateParts, IndicesDeleteParts, IndicesExistsParts, IndicesPutMappingParts},
    CreateParts, DeleteByQueryParts, DeleteParts, GetParts, IndexParts, MgetParts, OpenSearch,
    SearchParts,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use stac::Bbox;
use stac_api::Search;
use std::collections::{HashMap, HashSet};

pub const ROOT_CATALOG_ID: &str = "root";

/// Retries on optimistic-concurrency conflicts before giving up
/// (callers map `false` to HTTP 409).
const MAX_WRITE_RETRIES: usize = 5;

/// OpenSearch `_seq_no`/`_primary_term` stamp for conditional writes.
#[derive(Clone, Copy)]
pub struct DocVersion {
    pub seq_no: i64,
    pub primary_term: i64,
}

/// Extract the version stamp from a `_doc` GET response body.
fn doc_version(body: &Value) -> Option<DocVersion> {
    Some(DocVersion {
        seq_no: body["_seq_no"].as_i64()?,
        primary_term: body["_primary_term"].as_i64()?,
    })
}

/// Logical index names — `Store` prepends its index prefix
/// (`{prefix}-{logical}`). The default `stac` prefix maps onto the
/// historical `stac-catalogs` / `stac-items` / ... index names.
pub const HIERARCHY_INDEX: &str = "hierarchy";
pub const CATALOGS_INDEX: &str = "catalogs";
pub const COLLECTIONS_INDEX: &str = "collections";
pub const ITEMS_INDEX: &str = "items";

/// Safety cap on DAG depth when resolving descendants.
const MAX_DESCENDANT_DEPTH: usize = 25;
/// Cap on children fetched per query (proper pagination is future work).
const MAX_CHILDREN: usize = 10_000;
/// Default / max page size for item search.
pub const DEFAULT_SEARCH_LIMIT: u64 = 100;
pub const MAX_SEARCH_LIMIT: u64 = 10_000;

/// STAC node type tracked in the hierarchy DAG.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeKind {
    Catalog,
    Collection,
}

/// One document per node in `stac-hierarchy`, keyed by resource id.
/// Children are derived via `term` queries on `parents`, so link/unlink
/// only ever touches the child's document — no materialized DAG updates.
#[derive(Debug, Serialize, Deserialize)]
struct HierarchyNode {
    kind: NodeKind,
    #[serde(default)]
    parents: Vec<String>,
}

/// A hierarchy node as returned by children queries (id comes from `_id`).
#[derive(Debug)]
pub struct ChildNode {
    pub id: String,
    pub kind: NodeKind,
    pub parents: Vec<String>,
}

#[derive(Clone)]
pub struct Store {
    client: OpenSearch,
    prefix: String,
}

impl Store {
    pub fn connect(url: &str) -> Result<Self, opensearch::Error> {
        let url = Url::parse(url).expect("Invalid OPENSEARCH_URL");
        let pool = SingleNodeConnectionPool::new(url);
        let transport = TransportBuilder::new(pool).build()?;
        Ok(Self {
            client: OpenSearch::new(transport),
            prefix: "stac".to_string(),
        })
    }

    /// Use `{prefix}-{logical}` index names instead of `stac-*` — tests
    /// get fully isolated indices.
    pub fn with_index_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    /// The physical index name for a logical index constant.
    fn idx(&self, logical: &str) -> String {
        format!("{}-{}", self.prefix, logical)
    }

    /// Delete every `{prefix}-*` index — test teardown helper.
    pub async fn drop_indices(&self) -> Result<(), opensearch::Error> {
        let pattern = format!("{}-*", self.prefix);
        self.client
            .indices()
            .delete(IndicesDeleteParts::Index(&[&pattern]))
            .send()
            .await?;
        Ok(())
    }

    /// Create the STAC indices on startup if they don't already exist.
    pub async fn ensure_indices(&self) -> Result<(), opensearch::Error> {
        let indices = [
            (
                HIERARCHY_INDEX,
                json!({"properties": {
                    "kind": {"type": "keyword"},
                    "parents": {"type": "keyword"}
                }}),
            ),
            (
                CATALOGS_INDEX,
                json!({"properties": {"id": {"type": "keyword"}}}),
            ),
            (
                COLLECTIONS_INDEX,
                json!({"properties": {"id": {"type": "keyword"}}}),
            ),
            (
                ITEMS_INDEX,
                // long->double: with every collection's items in one index,
                // dynamic mapping would otherwise lock a property name to the
                // first numeric type seen and reject the other (e.g. one
                // collection's `eo:bands.center_wavelength` int colliding
                // with another's float).
                json!({
                    "dynamic_templates": [
                        {"ints_as_double": {
                            "match_mapping_type": "long",
                            "mapping": {"type": "double"}
                        }},
                        {"floats_as_double": {
                            "match_mapping_type": "double",
                            "mapping": {"type": "double"}
                        }}
                    ],
                    "properties": {
                        "id": {"type": "keyword"},
                        "collection": {"type": "keyword"},
                        "bbox": {"type": "float"},
                        "geometry": {"type": "geo_shape"},
                        "properties": {"properties": {
                            "datetime": {"type": "date"},
                            "start_datetime": {"type": "date"},
                            "end_datetime": {"type": "date"}
                        }}
                    }
                }),
            ),
        ];

        for (logical, mappings) in indices {
            let index = self.idx(logical);
            let exists = self
                .client
                .indices()
                .exists(IndicesExistsParts::Index(&[&index]))
                .send()
                .await?;
            if !exists.status_code().is_success() {
                self.client
                    .indices()
                    .create(IndicesCreateParts::Index(&index))
                    // 0 replicas: dev default is single-node; without this
                    // every index sits yellow with unassigned shards.
                    .body(json!({
                        "settings": {"number_of_replicas": 0},
                        "mappings": mappings
                    }))
                    .send()
                    .await?;
            } else {
                // Index exists: apply additive mapping updates. Type
                // changes to already-mapped fields (e.g. a legacy index
                // where dynamic mapping made `geometry` an object) are
                // rejected by OpenSearch — warn so the dev can drop and
                // recreate the index.
                let resp = self
                    .client
                    .indices()
                    .put_mapping(IndicesPutMappingParts::Index(&[&index]))
                    .body(mappings)
                    .send()
                    .await?;
                if !resp.status_code().is_success() {
                    eprintln!(
                        "warning: could not apply mapping updates to {index} \
                         (field-type conflicts need a reindex — drop the index \
                         and restart to rebuild it)"
                    );
                }
            }
        }
        Ok(())
    }

    // --- Hierarchy DAG ---

    /// Replace a node's parent set (Mode A create / full update).
    /// Orphan safety: an empty parent set is adopted by root.
    /// Returns `false` when the write kept losing optimistic-concurrency
    /// races — callers map that to a 409.
    pub async fn set_parents(
        &self,
        node_id: &str,
        parent_ids: Vec<String>,
        kind: NodeKind,
    ) -> Result<bool, opensearch::Error> {
        let mut parents = parent_ids;
        if parents.is_empty() {
            parents.push(ROOT_CATALOG_ID.to_string());
        }
        self.mutate_node(node_id, kind, move |node| {
            node.kind = kind;
            node.parents.clone_from(&parents);
        })
        .await
    }

    /// Add a single parent to a node (Mode B linking).
    /// Keeps the node's existing kind if it was already registered.
    /// `false` = retries exhausted (concurrent mutations).
    pub async fn link(
        &self,
        child_id: &str,
        parent_id: &str,
        kind: NodeKind,
    ) -> Result<bool, opensearch::Error> {
        self.mutate_node(child_id, kind, move |node| {
            apply_link(&mut node.parents, parent_id)
        })
        .await
    }

    /// Unlink a child from a parent; adopts the child under root if orphaned.
    /// `false` = retries exhausted (concurrent mutations).
    pub async fn unlink_and_adopt(
        &self,
        child_id: &str,
        parent_id: &str,
    ) -> Result<bool, opensearch::Error> {
        self.mutate_node(child_id, NodeKind::Catalog, move |node| {
            apply_unlink(&mut node.parents, parent_id)
        })
        .await
    }

    /// Read-modify-write a hierarchy node with optimistic concurrency:
    /// the mutation is replayed on a fresh doc each attempt, so concurrent
    /// edges merge instead of lost-updating. `false` = retries exhausted.
    async fn mutate_node(
        &self,
        node_id: &str,
        default_kind: NodeKind,
        mutate: impl Fn(&mut HierarchyNode),
    ) -> Result<bool, opensearch::Error> {
        for _ in 0..MAX_WRITE_RETRIES {
            let (mut node, version) =
                self.get_node_versioned(node_id).await?.unwrap_or_else(|| {
                    (
                        HierarchyNode {
                            kind: default_kind,
                            parents: vec![ROOT_CATALOG_ID.to_string()],
                        },
                        None,
                    )
                });
            mutate(&mut node);
            if self.put_node_versioned(node_id, &node, version).await? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Delete a node outright (disband). Since children are derived from
    /// `parents` queries, deleting the doc detaches it from every parent.
    /// Callers must unlink the node's own children first.
    pub async fn remove_node(&self, node_id: &str) -> Result<(), opensearch::Error> {
        let index = self.idx(HIERARCHY_INDEX);
        let resp = self
            .client
            .delete(DeleteParts::IndexId(&index, node_id))
            .refresh(opensearch::params::Refresh::WaitFor)
            .send()
            .await?;
        // 404 on an absent node is fine; anything worse is not
        if !resp.status_code().is_success() && resp.status_code() != StatusCode::NOT_FOUND {
            resp.error_for_status_code()?;
        }
        Ok(())
    }

    pub async fn get_parents(&self, node_id: &str) -> Result<Vec<String>, opensearch::Error> {
        Ok(self
            .get_node(node_id)
            .await?
            .map(|n| n.parents)
            .unwrap_or_default())
    }

    /// Direct child ids of a node (reverse `term` lookup on `parents`).
    pub async fn get_children(&self, node_id: &str) -> Result<Vec<String>, opensearch::Error> {
        Ok(self
            .search_children(node_id, None)
            .await?
            .into_iter()
            .map(|c| c.id)
            .collect())
    }

    /// Direct child ids of a node filtered by kind.
    pub async fn get_children_by_kind(
        &self,
        node_id: &str,
        kind: NodeKind,
    ) -> Result<Vec<String>, opensearch::Error> {
        Ok(self
            .search_children(node_id, Some(kind))
            .await?
            .into_iter()
            .map(|c| c.id)
            .collect())
    }

    /// Direct children with kind + parents — enough to render child links
    /// without a second round trip.
    pub async fn get_child_nodes(
        &self,
        node_id: &str,
        kind: Option<NodeKind>,
    ) -> Result<Vec<ChildNode>, opensearch::Error> {
        self.search_children(node_id, kind).await
    }

    async fn search_children(
        &self,
        node_id: &str,
        kind: Option<NodeKind>,
    ) -> Result<Vec<ChildNode>, opensearch::Error> {
        let mut must = vec![json!({"term": {"parents": node_id}})];
        if let Some(kind) = kind {
            must.push(json!({"term": {"kind": kind}}));
        }
        let index = self.idx(HIERARCHY_INDEX);
        let resp = self
            .client
            .search(SearchParts::Index(&[&index]))
            .body(json!({
                "size": MAX_CHILDREN,
                "query": {"bool": {"must": must}}
            }))
            .send()
            .await?;
        resp.error_for_status_code_ref()?;
        let body = resp.json::<Value>().await?;
        Ok(body["hits"]["hits"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|hit| {
                let node: HierarchyNode = serde_json::from_value(hit["_source"].clone()).ok()?;
                Some(ChildNode {
                    id: hit["_id"].as_str()?.to_string(),
                    kind: node.kind,
                    parents: node.parents,
                })
            })
            .collect())
    }

    /// Parents of many nodes in one mget (for link rendering on lists).
    pub async fn get_parents_many(
        &self,
        ids: &[String],
    ) -> Result<HashMap<String, Vec<String>>, opensearch::Error> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let index = self.idx(HIERARCHY_INDEX);
        let resp = self
            .client
            .mget(MgetParts::Index(&index))
            .body(json!({"ids": ids}))
            .send()
            .await?;
        resp.error_for_status_code_ref()?;
        let body = resp.json::<Value>().await?;
        Ok(body["docs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|d| d["found"] == true)
            .filter_map(|d| {
                let id = d["_id"].as_str()?.to_string();
                let node: HierarchyNode = serde_json::from_value(d["_source"].clone()).ok()?;
                Some((id, node.parents))
            })
            .collect())
    }

    /// Resolve descendant COLLECTION ids for scoped search via level-wise
    /// BFS over `parents`. Depth-capped and cycle-safe.
    pub async fn get_descendant_collections(
        &self,
        root_id: &str,
    ) -> Result<HashSet<String>, opensearch::Error> {
        let mut visited: HashSet<String> = [root_id.to_string()].into_iter().collect();
        let mut frontier = vec![root_id.to_string()];
        let mut collections = HashSet::new();

        for _ in 0..MAX_DESCENDANT_DEPTH {
            if frontier.is_empty() {
                break;
            }
            let index = self.idx(HIERARCHY_INDEX);
            let resp = self
                .client
                .search(SearchParts::Index(&[&index]))
                .body(json!({
                    "size": MAX_CHILDREN,
                    "query": {"terms": {"parents": frontier}}
                }))
                .send()
                .await?;
            resp.error_for_status_code_ref()?;
            let body = resp.json::<Value>().await?;
            let mut next = Vec::new();
            if let Some(hits) = body["hits"]["hits"].as_array() {
                for hit in hits {
                    let id = hit["_id"].as_str().unwrap_or_default().to_string();
                    if !visited.insert(id.clone()) {
                        continue;
                    }
                    let kind =
                        serde_json::from_value::<NodeKind>(hit["_source"]["kind"].clone()).ok();
                    if kind == Some(NodeKind::Collection) {
                        collections.insert(id.clone());
                    }
                    next.push(id);
                }
            }
            frontier = next;
        }
        Ok(collections)
    }

    // --- STAC document storage ---

    /// Unconditional index — for seed/tests only. Real write paths use
    /// `create_document`/`put_document`/`replace_document` so conflicts
    /// aren't silently lost.
    pub async fn index_document(
        &self,
        index: &str,
        id: &str,
        doc: impl Serialize,
    ) -> Result<(), opensearch::Error> {
        let index = self.idx(index);
        self.client
            .index(IndexParts::IndexId(&index, id))
            .body(doc)
            .refresh(opensearch::params::Refresh::WaitFor)
            .send()
            .await?
            .error_for_status_code()?;
        Ok(())
    }

    pub async fn get_document(
        &self,
        index: &str,
        id: &str,
    ) -> Result<Option<Value>, opensearch::Error> {
        Ok(self
            .get_document_versioned(index, id)
            .await?
            .map(|(doc, _)| doc))
    }

    /// Fetch a document with its `_seq_no`/`_primary_term` stamp —
    /// required for conditional (`if_seq_no`) writes.
    pub async fn get_document_versioned(
        &self,
        index: &str,
        id: &str,
    ) -> Result<Option<(Value, DocVersion)>, opensearch::Error> {
        let index = self.idx(index);
        let resp = self
            .client
            .get(GetParts::IndexId(&index, id))
            .send()
            .await?;
        if resp.status_code() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        resp.error_for_status_code_ref()?;
        let body = resp.json::<Value>().await?;
        Ok(doc_version(&body).map(|v| (body["_source"].clone(), v)))
    }

    /// Conditional write: fails (returns `false`) if the doc moved on
    /// since `version` was read. Other failures surface as errors.
    pub async fn index_document_versioned(
        &self,
        index: &str,
        id: &str,
        doc: impl Serialize,
        version: DocVersion,
    ) -> Result<bool, opensearch::Error> {
        let index = self.idx(index);
        let resp = self
            .client
            .index(IndexParts::IndexId(&index, id))
            .if_seq_no(version.seq_no)
            .if_primary_term(version.primary_term)
            .body(doc)
            .refresh(opensearch::params::Refresh::WaitFor)
            .send()
            .await?;
        Ok(match resp.status_code() {
            StatusCode::CONFLICT => false,
            s if s.is_success() => true,
            _ => {
                resp.error_for_status_code()?;
                unreachable!()
            }
        })
    }

    /// Replace an existing document with bounded optimistic-concurrency
    /// retries (PUT semantics — the body doesn't depend on the old doc,
    /// so a retry just re-stamps the same write). `false` = retries
    /// exhausted or the doc vanished mid-flight.
    pub async fn replace_document(
        &self,
        index: &str,
        id: &str,
        doc: &(impl Serialize + Sync),
    ) -> Result<bool, opensearch::Error> {
        for _ in 0..MAX_WRITE_RETRIES {
            let Some((_, version)) = self.get_document_versioned(index, id).await? else {
                // Deleted between the caller's 404 check and now.
                return Ok(false);
            };
            if self
                .index_document_versioned(index, id, doc, version)
                .await?
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Upsert (PUT semantics that may create). Existing doc → conditional
    /// write with retries; absent → `op_type=create`, and a raced create
    /// re-reads and conditional-writes on the next iteration.
    pub async fn put_document(
        &self,
        index: &str,
        id: &str,
        doc: &(impl Serialize + Sync),
    ) -> Result<bool, opensearch::Error> {
        for _ in 0..MAX_WRITE_RETRIES {
            let applied = match self.get_document_versioned(index, id).await? {
                Some((_, version)) => {
                    self.index_document_versioned(index, id, doc, version)
                        .await?
                }
                None => self.create_document(index, id, doc).await?,
            };
            if applied {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Create-if-absent (`op_type=create`). `false` = a doc with this id
    /// exists — the atomic version of the callers' get-then-409 check.
    pub async fn create_document(
        &self,
        index: &str,
        id: &str,
        doc: &(impl Serialize + Sync),
    ) -> Result<bool, opensearch::Error> {
        let index = self.idx(index);
        let resp = self
            .client
            .create(CreateParts::IndexId(&index, id))
            .body(doc)
            .refresh(opensearch::params::Refresh::WaitFor)
            .send()
            .await?;
        Ok(match resp.status_code() {
            StatusCode::CONFLICT => false,
            s if s.is_success() => true,
            _ => {
                resp.error_for_status_code()?;
                unreachable!()
            }
        })
    }

    /// Delete a document. `false` = it was already gone (raced delete).
    pub async fn delete_document(&self, index: &str, id: &str) -> Result<bool, opensearch::Error> {
        let index = self.idx(index);
        let resp = self
            .client
            .delete(DeleteParts::IndexId(&index, id))
            .refresh(opensearch::params::Refresh::WaitFor)
            .send()
            .await?;
        Ok(match resp.status_code() {
            StatusCode::NOT_FOUND => false,
            s if s.is_success() => true,
            _ => {
                resp.error_for_status_code()?;
                unreachable!()
            }
        })
    }

    /// Delete all items belonging to a collection (collection teardown).
    pub async fn delete_items_by_collection(
        &self,
        collection_id: &str,
    ) -> Result<(), opensearch::Error> {
        let index = self.idx(ITEMS_INDEX);
        self.client
            .delete_by_query(DeleteByQueryParts::Index(&[&index]))
            .body(json!({"query": {"term": {"collection": collection_id}}}))
            .refresh(true)
            .send()
            .await?
            .error_for_status_code()?;
        Ok(())
    }

    /// Fetch many documents by id (missing ids are skipped).
    pub async fn get_documents(
        &self,
        index: &str,
        ids: &[String],
    ) -> Result<Vec<Value>, opensearch::Error> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let index = self.idx(index);
        let resp = self
            .client
            .mget(MgetParts::Index(&index))
            .body(json!({"ids": ids}))
            .send()
            .await?;
        resp.error_for_status_code_ref()?;
        let body = resp.json::<Value>().await?;
        Ok(body["docs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|d| d["found"] == true)
            .map(|d| d["_source"].clone())
            .collect())
    }

    /// Item search scoped to a set of collections.
    /// Returns (item docs, total_matched) — sources pass through verbatim
    /// since ItemCollection supports the fields extension.
    /// Translates collections/ids/bbox/intersects/datetime/limit plus
    /// `offset` (unmodeled, arrives via `additional_fields`); sortby,
    /// fields and cursor/deep pagination remain future work.
    pub async fn search_items(
        &self,
        collections: &[String],
        search: &Search,
    ) -> Result<(Vec<Map<String, Value>>, u64), opensearch::Error> {
        let limit = search
            .items
            .limit
            .unwrap_or(DEFAULT_SEARCH_LIMIT)
            .min(MAX_SEARCH_LIMIT);
        let offset = search_offset(search);

        let mut filter = vec![json!({"terms": {"collection": collections}})];
        if !search.ids.is_empty() {
            // _id == item.id (docs are indexed under their own id)
            filter.push(json!({"ids": {"values": search.ids}}));
        }
        if let Some(bbox) = &search.items.bbox {
            filter.push(bbox_filter(bbox));
        }
        if let Some(geom) = &search.intersects {
            // geojson::Geometry serializes to a GeoJSON shape literal
            filter.push(json!({"geo_shape": {"geometry": {
                "shape": serde_json::to_value(geom).unwrap_or_default(),
                "relation": "intersects"
            }}}));
        }
        if let Some(datetime) = &search.items.datetime {
            filter.push(datetime_filter(datetime));
        }

        // sortby -> OpenSearch `sort` clauses; `_id` is appended as a
        // tiebreaker so paging is deterministic. Fields are trimmed because
        // `?sortby=+x` URL-decodes to a leading space; `unmapped_type` keeps
        // sorting by a not-yet-indexed property from raising a shard error.
        let mut sort: Vec<Value> = search
            .items
            .sortby
            .iter()
            .map(|s| {
                json!({sort_field(s.field.trim()): {
                    "order": s.direction,
                    "missing": "_last",
                    "unmapped_type": "keyword"
                }})
            })
            .collect();
        if !sort.is_empty() {
            sort.push(json!({"_id": {"order": "asc"}}));
        }

        let index = self.idx(ITEMS_INDEX);
        let mut body = json!({
            "size": limit,
            "from": offset,
            "track_total_hits": true,
            "query": {"bool": {"filter": filter}}
        });
        if !sort.is_empty() {
            body["sort"] = json!(sort);
        }
        let resp = self
            .client
            .search(SearchParts::Index(&[&index]))
            .body(body)
            .send()
            .await?;
        resp.error_for_status_code_ref()?;
        let body = resp.json::<Value>().await?;
        let total = body["hits"]["total"]["value"].as_u64().unwrap_or(0);
        let items = body["hits"]["hits"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|h| h["_source"].as_object().cloned())
            .collect();
        Ok((items, total))
    }

    // --- node doc helpers ---

    async fn get_node(&self, node_id: &str) -> Result<Option<HierarchyNode>, opensearch::Error> {
        Ok(self
            .get_node_versioned(node_id)
            .await?
            .map(|(node, _)| node))
    }

    async fn get_node_versioned(
        &self,
        node_id: &str,
    ) -> Result<Option<(HierarchyNode, Option<DocVersion>)>, opensearch::Error> {
        let index = self.idx(HIERARCHY_INDEX);
        let resp = self
            .client
            .get(GetParts::IndexId(&index, node_id))
            .send()
            .await?;
        if resp.status_code() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let body = resp.json::<Value>().await?;
        Ok(serde_json::from_value(body["_source"].clone())
            .ok()
            .map(|node| (node, doc_version(&body))))
    }

    /// Optimistic-concurrency write. `None` version = create-if-absent
    /// (`op_type=create`, conflicts if a node appeared meanwhile).
    /// Returns `false` on a version conflict — retry with a fresh read.
    async fn put_node_versioned(
        &self,
        node_id: &str,
        node: &HierarchyNode,
        version: Option<DocVersion>,
    ) -> Result<bool, opensearch::Error> {
        let index = self.idx(HIERARCHY_INDEX);
        let resp = match version {
            Some(v) => {
                self.client
                    .index(IndexParts::IndexId(&index, node_id))
                    .if_seq_no(v.seq_no)
                    .if_primary_term(v.primary_term)
                    .body(node)
                    .refresh(opensearch::params::Refresh::WaitFor)
                    .send()
                    .await?
            }
            None => {
                self.client
                    .create(CreateParts::IndexId(&index, node_id))
                    .body(node)
                    .refresh(opensearch::params::Refresh::WaitFor)
                    .send()
                    .await?
            }
        };
        Ok(match resp.status_code() {
            StatusCode::CONFLICT => false,
            s if s.is_success() => true,
            _ => {
                resp.error_for_status_code()?;
                unreachable!()
            }
        })
    }
}

/// sortby field -> document field. Top-level item fields pass through;
/// anything else is a `properties.*` member (bare `datetime` included).
fn sort_field(field: &str) -> String {
    match field {
        "id" | "collection" | "bbox" | "geometry" | "type" => field.to_string(),
        f if f.contains('.') => f.to_string(),
        f => format!("properties.{f}"),
    }
}

/// Page offset — `offset` isn't a modeled `Search` field, it arrives via
/// `additional_fields` (same trick stac-server's duckdb backend uses).
/// POST bodies carry it as a number; GET query params arrive as strings.
pub fn search_offset(search: &Search) -> u64 {
    search
        .items
        .additional_fields
        .get("offset")
        .and_then(|v| {
            v.as_u64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        })
        .unwrap_or(0)
}

/// STAC `bbox` -> geo_shape envelope query (intersects semantics).
/// Envelope corners are [[min_lon, max_lat], [max_lon, min_lat]].
fn bbox_filter(bbox: &Bbox) -> Value {
    json!({"geo_shape": {"geometry": {
        "shape": {"type": "envelope", "coordinates": [
            [bbox.xmin(), bbox.ymax()],
            [bbox.xmax(), bbox.ymin()]
        ]},
        "relation": "intersects"
    }}})
}

/// An interval endpoint: empty / ".." / "null" means open-ended.
fn dt_bound(s: &str) -> Option<&str> {
    match s {
        "" | ".." | "null" => None,
        v => Some(v),
    }
}

/// STAC `datetime` -> temporal filter. Covers both instant items
/// (`properties.datetime`) and ranged items (`datetime: null` +
/// `start_datetime`/`end_datetime`), matching either if it overlaps the
/// query interval. A bare instant is treated as a zero-width interval.
fn datetime_filter(datetime: &str) -> Value {
    let (start, end) = match datetime.split_once('/') {
        Some((s, e)) => (dt_bound(s), dt_bound(e)),
        None => (dt_bound(datetime), dt_bound(datetime)),
    };

    let mut instant_range = Map::new();
    if let Some(s) = start {
        instant_range.insert("gte".into(), json!(s));
    }
    if let Some(e) = end {
        instant_range.insert("lte".into(), json!(e));
    }
    if instant_range.is_empty() {
        return json!({"match_all": {}});
    }

    // Ranged items overlap [start, end] iff start_datetime <= end AND
    // end_datetime >= start (open bounds are simply omitted).
    let mut ranged = Vec::new();
    if let Some(e) = end {
        ranged.push(json!({"range": {"properties.start_datetime": {"lte": e}}}));
    }
    if let Some(s) = start {
        ranged.push(json!({"range": {"properties.end_datetime": {"gte": s}}}));
    }
    ranged.push(json!({"bool": {"must_not": {"exists": {"field": "properties.datetime"}}}}));

    json!({"bool": {"should": [
        {"range": {"properties.datetime": instant_range}},
        {"bool": {"filter": ranged}}
    ], "minimum_should_match": 1}})
}

/// Mode B link semantics on a node's parent list: linking to a real
/// catalog drops the implicit root parent; duplicates are ignored.
fn apply_link(parents: &mut Vec<String>, parent_id: &str) {
    if parent_id != ROOT_CATALOG_ID {
        parents.retain(|p| p != ROOT_CATALOG_ID);
    }
    if !parents.iter().any(|p| p == parent_id) {
        parents.push(parent_id.to_string());
    }
}

/// Unlink semantics: removing the last parent triggers root adoption.
fn apply_unlink(parents: &mut Vec<String>, parent_id: &str) {
    parents.retain(|p| p != parent_id);
    if parents.is_empty() {
        parents.push(ROOT_CATALOG_ID.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_link_drops_implicit_root() {
        let mut parents = vec![ROOT_CATALOG_ID.to_string()];
        apply_link(&mut parents, "real-parent");
        assert_eq!(parents, vec!["real-parent"]);
    }

    #[test]
    fn test_link_keeps_explicit_parents() {
        let mut parents = vec!["a".to_string(), ROOT_CATALOG_ID.to_string()];
        apply_link(&mut parents, "b");
        assert_eq!(parents, vec!["a", "b"]);
    }

    #[test]
    fn test_unlink_adopts_orphan_to_root() {
        let mut parents = vec!["parent".to_string()];
        apply_unlink(&mut parents, "parent");
        assert_eq!(parents, vec![ROOT_CATALOG_ID]);
    }

    #[test]
    fn test_unlink_keeps_remaining_parents() {
        let mut parents = vec!["a".to_string(), "b".to_string()];
        apply_unlink(&mut parents, "a");
        assert_eq!(parents, vec!["b"]);
    }

    #[test]
    fn test_sort_field_mapping() {
        assert_eq!(sort_field("id"), "id");
        assert_eq!(sort_field("collection"), "collection");
        assert_eq!(sort_field("datetime"), "properties.datetime");
        assert_eq!(sort_field("properties.created"), "properties.created");
        assert_eq!(sort_field("eo:cloud_cover"), "properties.eo:cloud_cover");
    }

    #[test]
    fn test_bbox_filter_envelope() {
        let q = bbox_filter(&Bbox::new(-122.6, 36.9, -120.3, 38.2));
        let shape = &q["geo_shape"]["geometry"]["shape"];
        assert_eq!(shape["type"], "envelope");
        assert_eq!(
            shape["coordinates"],
            json!([[-122.6, 38.2], [-120.3, 36.9]])
        );
        assert_eq!(q["geo_shape"]["geometry"]["relation"], "intersects");
    }

    #[test]
    fn test_datetime_filter_instant() {
        let q = datetime_filter("2023-06-15T00:00:00Z");
        let should = q["bool"]["should"].as_array().unwrap();
        // instant items: exact instant; ranged items: start<=v<=end
        assert_eq!(
            should[0]["range"]["properties.datetime"],
            json!({"gte": "2023-06-15T00:00:00Z", "lte": "2023-06-15T00:00:00Z"})
        );
    }

    #[test]
    fn test_datetime_filter_interval() {
        let q = datetime_filter("2023-06-01/2023-07-01");
        let should = q["bool"]["should"].as_array().unwrap();
        assert_eq!(
            should[0]["range"]["properties.datetime"],
            json!({"gte": "2023-06-01", "lte": "2023-07-01"})
        );
        let ranged = should[1]["bool"]["filter"].as_array().unwrap();
        assert_eq!(
            ranged[0]["range"]["properties.start_datetime"],
            json!({"lte": "2023-07-01"})
        );
        assert_eq!(
            ranged[1]["range"]["properties.end_datetime"],
            json!({"gte": "2023-06-01"})
        );
    }

    #[test]
    fn test_datetime_filter_open_interval() {
        // "2023-06-01/.." -> only a lower bound on both branches
        let q = datetime_filter("2023-06-01/..");
        let should = q["bool"]["should"].as_array().unwrap();
        assert_eq!(
            should[0]["range"]["properties.datetime"],
            json!({"gte": "2023-06-01"})
        );
        let ranged = should[1]["bool"]["filter"].as_array().unwrap();
        assert_eq!(ranged.len(), 2); // end_datetime gte + no-datetime guard
        assert_eq!(
            ranged[0]["range"]["properties.end_datetime"],
            json!({"gte": "2023-06-01"})
        );
    }
}
