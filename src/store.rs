// src/store.rs
use opensearch::{
    http::{
        transport::{SingleNodeConnectionPool, TransportBuilder},
        StatusCode, Url,
    },
    indices::{
        IndicesCreateParts, IndicesDeleteParts, IndicesExistsParts, IndicesGetMappingParts,
        IndicesPutMappingParts,
    },
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
    /// `USE_DATETIME` (default true): when false, an item carrying both
    /// `properties.datetime` and `start_datetime`/`end_datetime` is
    /// filtered on the range only — upstream's ranged-item semantics.
    use_datetime: bool,
}

impl Store {
    pub fn connect(url: &str) -> Result<Self, opensearch::Error> {
        let url = Url::parse(url).expect("Invalid OPENSEARCH_URL");
        let pool = SingleNodeConnectionPool::new(url);
        let transport = TransportBuilder::new(pool).build()?;
        Ok(Self {
            client: OpenSearch::new(transport),
            prefix: "stac".to_string(),
            use_datetime: true,
        })
    }

    /// Use `{prefix}-{logical}` index names instead of `stac-*` — tests
    /// get fully isolated indices.
    pub fn with_index_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    /// `USE_DATETIME=false` — prefer `start_datetime`/`end_datetime` over
    /// `datetime` whenever both are present on an item.
    pub fn with_use_datetime(mut self, use_datetime: bool) -> Self {
        self.use_datetime = use_datetime;
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
                json!({"properties": {
                    "id": {"type": "keyword"},
                    // text+keyword: free-text (`q`) needs analysis; term/
                    // sort on strings needs the keyword sub-field.
                    "title": {"type": "text", "fields": {"keyword": {"type": "keyword", "ignore_above": 256}}},
                    "description": {"type": "text", "fields": {"keyword": {"type": "keyword", "ignore_above": 256}}},
                    "keywords": {"type": "text", "fields": {"keyword": {"type": "keyword", "ignore_above": 256}}},
                    // collections-search: temporal-extent overlap +
                    // `bbox_shape` (injected at write time, upstream
                    // does the same for geo queries on collections).
                    "extent": {"properties": {"temporal": {"properties": {
                        "interval": {"type": "date"}
                    }}}},
                    "bbox_shape": {"type": "geo_shape"}
                }}),
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

    /// Raw `GET /{index}/_mapping` for a logical index — used by the
    /// `/queryables` endpoints to discover dynamic item properties.
    pub async fn get_index_mapping(&self, logical: &str) -> Result<Value, opensearch::Error> {
        let index = self.idx(logical);
        self.client
            .indices()
            .get_mapping(IndicesGetMappingParts::Index(&[&index]))
            .send()
            .await?
            .error_for_status_code()?
            .json()
            .await
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

    /// Stable 8-hex hash (FNV-1a) of the physical index name — feeds
    /// ETags so a tag can only be interpreted against the index that
    /// produced it.
    pub fn index_hash(&self, logical: &str) -> String {
        let mut h: u64 = 0xcbf29ce484222325;
        for b in self.idx(logical).as_bytes() {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x100000001b3);
        }
        format!("{h:016x}")[..8].to_string()
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

    /// Delete conditioned on `version` (atomic check-and-delete).
    /// `false` = the doc moved on (or is already gone).
    pub async fn delete_document_versioned(
        &self,
        index: &str,
        id: &str,
        version: DocVersion,
    ) -> Result<bool, opensearch::Error> {
        let index = self.idx(index);
        let resp = self
            .client
            .delete(DeleteParts::IndexId(&index, id))
            .if_seq_no(version.seq_no)
            .if_primary_term(version.primary_term)
            .refresh(opensearch::params::Refresh::WaitFor)
            .send()
            .await?;
        Ok(match resp.status_code() {
            StatusCode::NOT_FOUND | StatusCode::CONFLICT => false,
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
    /// `offset` (unmodeled, arrives via `additional_fields`); `extra_filters`
    /// carries caller-built clauses (`query`/`filter` extensions — see
    /// [`stacql_clauses`]/[`cql2_clause`]). Cursor/deep pagination remains
    /// future work.
    pub async fn search_items(
        &self,
        collections: &[String],
        search: &Search,
        extra_filters: Vec<Value>,
    ) -> Result<(Vec<Map<String, Value>>, u64), opensearch::Error> {
        let limit = search
            .items
            .limit
            .unwrap_or(DEFAULT_SEARCH_LIMIT)
            .min(MAX_SEARCH_LIMIT);
        let offset = search_offset(search);

        let mut filter = vec![json!({"terms": {"collection": collections}})];
        if !search.ids.is_empty() {
            filter.push(json!({"terms": {"id": search.ids}}));
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
            filter.push(datetime_filter(datetime, self.use_datetime));
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
                json!({item_field(s.field.trim()): {
                    "order": s.direction,
                    "missing": "_last",
                    "unmapped_type": "keyword"
                }})
            })
            .collect();
        if !sort.is_empty() {
            sort.push(json!({"_id": {"order": "asc"}}));
        }

        filter.extend(extra_filters);

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

    /// Collection search (`/collections-search` and the extended
    /// `/collections`): filters are already-built OpenSearch clauses
    /// (q/filter/query/datetime/bbox); `sort` is a list of
    /// `{field: {"order": dir}}` — the `id` asc tiebreak is appended
    /// here, matching upstream's `format_collection_sort_shared`.
    /// Returns (docs, total_matched).
    pub async fn search_collections(
        &self,
        filters: Vec<Value>,
        sort: Vec<(String, String)>,
        limit: u64,
        offset: u64,
    ) -> Result<(Vec<Value>, u64), opensearch::Error> {
        let mut sort_json: Vec<Value> = sort
            .iter()
            .map(|(field, dir)| {
                json!({collection_sort_field(field): {
                    "order": dir,
                    "missing": "_last",
                    "unmapped_type": "keyword"
                }})
            })
            .collect();
        if !sort_json
            .iter()
            .any(|s| s.as_object().is_some_and(|o| o.contains_key("id")))
        {
            sort_json.push(json!({"id": {"order": "asc"}}));
        }
        let index = self.idx(COLLECTIONS_INDEX);
        let body = json!({
            "size": limit,
            "from": offset,
            "track_total_hits": true,
            "query": {"bool": {"filter": filters}},
            "sort": sort_json,
        });
        let resp = self
            .client
            .search(SearchParts::Index(&[&index]))
            .body(body)
            .send()
            .await?;
        resp.error_for_status_code_ref()?;
        let body = resp.json::<Value>().await?;
        let total = body["hits"]["total"]["value"].as_u64().unwrap_or(0);
        let docs = body["hits"]["hits"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|h| h["_source"].as_object().cloned())
            .map(Value::Object)
            .collect();
        Ok((docs, total))
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

/// Fields extension (item-search#fields / ogcapi-features#fields): apply
/// include/exclude to a returned document, ported from upstream sfeos
/// `filter_fields`. `*` in a path segment is a glob within one key name.
/// Include runs first; excludes then apply to the included result. When the
/// include set produces an empty document only the identity keys
/// (`id`, `collection`) are returned.
pub fn filter_fields(
    item: &mut serde_json::Map<String, Value>,
    include: &[String],
    exclude: &[String],
) {
    if include.is_empty() && exclude.is_empty() {
        return;
    }
    let mut clean = if include.is_empty() {
        item.clone()
    } else {
        include_fields(item, include)
    };
    if clean.is_empty() {
        for key in ["id", "collection"] {
            if let Some(v) = item.get(key) {
                clean.insert(key.to_string(), v.clone());
            }
        }
    }
    exclude_fields(&mut clean, exclude);
    *item = clean;
}

/// One `*` wildcard segment matched against a single key name (any chars).
fn segment_match(pattern: &str, key: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let (p, s) = (pattern.as_bytes(), key.as_bytes());
    let (mut pi, mut si, mut star, mut ss) = (0usize, 0usize, usize::MAX, 0usize);
    while si < s.len() {
        if pi < p.len() && p[pi] == s[si] {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = pi;
            ss = si;
            pi += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            ss += 1;
            si = ss;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

/// Deep-merge `from` into `to` (nested objects union, scalars overwrite).
fn deep_merge(to: &mut serde_json::Map<String, Value>, from: serde_json::Map<String, Value>) {
    for (k, v) in from {
        match (to.get_mut(&k), v) {
            (Some(Value::Object(t)), Value::Object(f)) => deep_merge(t, f),
            (_, v) => {
                to.insert(k, v);
            }
        }
    }
}

fn include_fields(
    source: &serde_json::Map<String, Value>,
    fields: &[String],
) -> serde_json::Map<String, Value> {
    let mut clean = serde_json::Map::new();
    for key_path in fields {
        if key_path.contains('*') {
            let parts: Vec<&str> = key_path.split('.').collect();
            let value = recursive_include(source, &parts);
            deep_merge(&mut clean, value);
            continue;
        }
        let parts: Vec<&str> = key_path.split('.').collect();
        let root = parts[0];
        if let Some(val) = source.get(root) {
            if val.is_object() && parts.len() > 1 {
                let sub = include_fields(val.as_object().unwrap(), &[parts[1..].join(".")]);
                match clean.get_mut(root) {
                    Some(Value::Object(existing)) => deep_merge(existing, sub),
                    _ => {
                        clean.insert(root.to_string(), Value::Object(sub));
                    }
                }
            } else {
                clean.insert(root.to_string(), val.clone());
            }
        }
    }
    clean
}

fn recursive_include(
    source: &serde_json::Map<String, Value>,
    parts: &[&str],
) -> serde_json::Map<String, Value> {
    if parts.is_empty() {
        return source.clone();
    }
    let mut result = serde_json::Map::new();
    for (key, val) in source {
        if !segment_match(parts[0], key) {
            continue;
        }
        if parts.len() > 1 {
            if let Value::Object(inner) = val {
                let sub = recursive_include(inner, &parts[1..]);
                if !sub.is_empty() {
                    result.insert(key.clone(), Value::Object(sub));
                }
            }
        } else {
            result.insert(key.clone(), val.clone());
        }
    }
    result
}

fn exclude_fields(source: &mut serde_json::Map<String, Value>, fields: &[String]) {
    for key_path in fields {
        let parts: Vec<&str> = key_path.split('.').collect();
        if key_path.contains('*') {
            recursive_exclude(source, &parts);
            continue;
        }
        let root = parts[0];
        let mut prune = false;
        if let Some(val) = source.get_mut(root) {
            if val.is_object() && parts.len() > 1 {
                exclude_fields(val.as_object_mut().unwrap(), &[parts[1..].join(".")]);
                prune = val.as_object().map(|o| o.is_empty()).unwrap_or(false);
            } else {
                source.remove(root);
            }
        }
        if prune {
            source.remove(root);
        }
    }
}

fn recursive_exclude(source: &mut serde_json::Map<String, Value>, parts: &[&str]) {
    if parts.is_empty() {
        return;
    }
    let keys: Vec<String> = source
        .keys()
        .filter(|k| segment_match(parts[0], k))
        .cloned()
        .collect();
    for key in keys {
        if parts.len() > 1 {
            let mut prune = false;
            if let Some(Value::Object(inner)) = source.get_mut(&key) {
                recursive_exclude(inner, &parts[1..]);
                prune = inner.is_empty();
            }
            if prune {
                source.remove(&key);
            }
        } else {
            source.remove(&key);
        }
    }
}

/// Field name -> document field. Well-known top-level item fields pass
/// through, as does any dotted path (`properties.x`, `assets.x`);
/// anything else is a `properties.*` member (bare `datetime` included).
/// Shared by `sortby`, `query`, and `filter` field resolution.
/// JSON Schema `properties` for `/queryables`, discovered from an index
/// mapping. Walks `properties` recursively — leaf fields become dotted
/// `a.b.c` names with `properties.`/`assets.` prefixes stripped
/// (upstream sfeos semantics). An `excluded` entry drops a subtree and
/// matches with or without the `properties.` prefix.
pub fn queryable_properties(mapping: &Value, excluded: &[String]) -> Map<String, Value> {
    let excluded: Vec<String> = excluded
        .iter()
        .flat_map(|e| {
            let e = e.trim();
            if e.is_empty() {
                Vec::new()
            } else if let Some(bare) = e.strip_prefix("properties.") {
                vec![e.to_string(), bare.to_string()]
            } else {
                vec![e.to_string(), format!("properties.{e}")]
            }
        })
        .collect();
    let is_excluded = |fqn: &str| {
        excluded
            .iter()
            .any(|p| fqn == p || fqn.starts_with(&format!("{p}.")))
    };

    let mut props = Map::new();
    let mut stack: Vec<(String, &Value)> = mapping
        .get("properties")
        .and_then(Value::as_object)
        .map(|o| o.iter().map(|(k, v)| (k.clone(), v)).collect())
        .unwrap_or_default();
    while let Some((fqn, def)) = stack.pop() {
        let enabled = def.get("enabled").and_then(Value::as_bool).unwrap_or(true);
        if !enabled || is_excluded(&fqn) {
            continue;
        }
        if let Some(nested) = def.get("properties").and_then(Value::as_object) {
            stack.extend(nested.iter().map(|(k, v)| (format!("{fqn}.{k}"), v)));
        }
        if def.get("type").and_then(Value::as_str).is_none() {
            continue;
        }
        let name = fqn
            .strip_prefix("properties.")
            .or_else(|| fqn.strip_prefix("assets."))
            .unwrap_or(&fqn);
        props
            .entry(name.to_string())
            .or_insert_with(|| os_type_schema(def["type"].as_str().unwrap_or_default()));
    }
    props
}

/// OpenSearch field type -> JSON Schema fragment for a queryable.
fn os_type_schema(ty: &str) -> Value {
    match ty {
        "keyword" | "text" | "wildcard" | "ip" | "version" => json!({"type": "string"}),
        "date" | "date_nanos" => json!({"type": "string", "format": "date-time"}),
        "long" | "integer" | "short" | "byte" | "unsigned_long" => json!({"type": "integer"}),
        "double" | "float" | "half_float" | "scaled_float" => json!({"type": "number"}),
        "boolean" => json!({"type": "boolean"}),
        _ => json!({}),
    }
}

fn item_field(field: &str) -> String {
    match field {
        "id" | "collection" | "bbox" | "geometry" | "type" => field.to_string(),
        f if f.contains('.') => f.to_string(),
        f => format!("properties.{f}"),
    }
}

/// Exact-match field for term/terms/wildcard clauses: dynamic string
/// properties land on `text` + `.keyword` (OpenSearch's default dynamic
/// mapping), so string comparisons must target the keyword sub-field.
/// Explicitly-mapped keywords (`id`, `collection`) and dates keep the
/// plain field.
fn exact_field(field: &str, value: &Value) -> String {
    if value.is_string()
        && field.starts_with("properties.")
        && !matches!(
            field,
            "properties.datetime" | "properties.start_datetime" | "properties.end_datetime"
        )
    {
        format!("{field}.keyword")
    } else {
        field.to_string()
    }
}

/// Collection docs have no `properties.*` — bare names resolve top-level.
fn collection_field(field: &str) -> String {
    field.to_string()
}

/// Sort field for collections-search: string fields sort on their
/// `.keyword` sub-field; `id`/`datetime` are the mapped exceptions
/// (`datetime` sorts on the temporal extent, upstream convention).
fn collection_sort_field(field: &str) -> String {
    match field {
        "id" => "id".to_string(),
        "datetime" => "extent.temporal.interval".to_string(),
        f => format!("{f}.keyword"),
    }
}

/// Same `.keyword` rule as [`exact_field`], rooted at collection fields:
/// `id` is a mapped keyword and `extent.*` holds dates/numbers; every
/// other string field is dynamic `text` + `.keyword`.
fn collection_exact_field(field: &str, value: &Value) -> String {
    if value.is_string()
        && field != "id"
        && !field.starts_with("extent.")
        && !field.ends_with(".keyword")
    {
        format!("{field}.keyword")
    } else {
        field.to_string()
    }
}

/// Field-name translation for CQL2/STACQL clauses — item vs collection.
#[derive(Clone, Copy)]
pub struct FieldSpec {
    /// Queryable name -> index field (`platform` -> `properties.platform`
    /// for items, `platform` -> `platform` for collections).
    pub map: fn(&str) -> String,
    /// (index field, value) -> exact-match field (`.keyword` for
    /// dynamic strings).
    pub exact: fn(&str, &Value) -> String,
}

pub const ITEM_FIELDS: FieldSpec = FieldSpec {
    map: item_field,
    exact: exact_field,
};

pub const COLLECTION_FIELDS: FieldSpec = FieldSpec {
    map: collection_field,
    exact: collection_exact_field,
};

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
/// `use_datetime=false`: items carrying a range ignore `datetime`
/// entirely (upstream `USE_DATETIME` semantics).
fn datetime_filter(datetime: &str, use_datetime: bool) -> Value {
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

    let instant_clause = if use_datetime {
        // Instant items: `datetime` matches and no range is present.
        // Ranged-only items fall to the ranged clause, which excludes
        // `datetime` so an item never matches on both.
        ranged.push(json!({"bool": {"must_not": {"exists": {"field": "properties.datetime"}}}}));
        json!({"range": {"properties.datetime": instant_range}})
    } else {
        // `datetime` counts only when NO range fields are present — a
        // ranged item is filtered on its range alone.
        json!({"bool": {"filter": [
            {"range": {"properties.datetime": instant_range}},
            {"bool": {"must_not": {"exists": {"field": "properties.start_datetime"}}}},
            {"bool": {"must_not": {"exists": {"field": "properties.end_datetime"}}}}
        ]}})
    };

    json!({"bool": {"should": [
        instant_clause,
        {"bool": {"filter": ranged}}
    ], "minimum_should_match": 1}})
}

/// STAC `query` extension -> OpenSearch filter clauses.
/// `{"eo:cloud_cover": {"lt": 10}}` — bare names resolve under
/// `properties.*` (same mapping upstream applies), `eq`/`ne`/`neq`/
/// `gt`/`gte`/`lt`/`lte`/`in`/`contains` operators supported.
pub fn stacql_clauses(query: &Map<String, Value>) -> Result<Vec<Value>, String> {
    stacql_clauses_with(query, ITEM_FIELDS)
}

pub fn stacql_clauses_with(
    query: &Map<String, Value>,
    spec: FieldSpec,
) -> Result<Vec<Value>, String> {
    let mut clauses = Vec::new();
    for (name, expr) in query {
        let ops = expr
            .as_object()
            .ok_or_else(|| format!("query.{name} must be an object of operators"))?;
        let field = (spec.map)(name);
        for (op, value) in ops {
            clauses.push(stacql_clause(&field, op, value, spec)?);
        }
    }
    Ok(clauses)
}

fn stacql_clause(field: &str, op: &str, value: &Value, spec: FieldSpec) -> Result<Value, String> {
    Ok(match op {
        "eq" | "contains" => json!({"term": {(spec.exact)(field, value): value}}),
        "ne" | "neq" => {
            json!({"bool": {"must_not": [{"term": {(spec.exact)(field, value): value}}]}})
        }
        "gt" | "gte" | "lt" | "lte" => json!({"range": {field: {op: value}}}),
        "in" => match value {
            Value::Array(values) => {
                let field = values
                    .first()
                    .map(|v| (spec.exact)(field, v))
                    .unwrap_or_else(|| field.to_string());
                json!({"terms": {field: value}})
            }
            _ => json!({"term": {(spec.exact)(field, value): value}}),
        },
        other => return Err(format!("unsupported query operator '{other}'")),
    })
}

/// CQL2 `filter` (json or text form) -> an OpenSearch clause.
/// Covers basic-cql2 (comparisons, and/or/not, IS NULL), advanced
/// comparisons (LIKE, BETWEEN, IN) and the basic spatial predicates;
/// anything else is a 400 via the caller.
pub fn cql2_clause(filter: &stac_api::Filter) -> Result<Value, String> {
    cql2_clause_with(filter, ITEM_FIELDS)
}

pub fn cql2_clause_with(filter: &stac_api::Filter, spec: FieldSpec) -> Result<Value, String> {
    let expr = cql2::Expr::try_from(filter.clone()).map_err(|e| e.to_string())?;
    expr_clause(&expr, spec)
}

fn expr_clause(expr: &cql2::Expr, spec: FieldSpec) -> Result<Value, String> {
    use cql2::Expr;
    match expr {
        Expr::Bool(true) => Ok(json!({"match_all": {}})),
        Expr::Bool(false) => Ok(json!({"match_none": {}})),
        Expr::Operation { op, args } => match op.as_str() {
            "and" | "or" => {
                let clauses = args
                    .iter()
                    .map(|a| expr_clause(a, spec))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(if op == "and" {
                    json!({"bool": {"filter": clauses}})
                } else {
                    json!({"bool": {"should": clauses, "minimum_should_match": 1}})
                })
            }
            "not" => {
                let [a] = args.as_slice() else {
                    return Err("'not' expects one operand".into());
                };
                Ok(json!({"bool": {"must_not": [expr_clause(a, spec)?]}}))
            }
            "=" | "<>" | "<" | "<=" | ">" | ">=" => comparison_clause(op, args, spec),
            "in" => in_clause(args, spec),
            "between" => between_clause(args, spec),
            "like" => like_clause(args, spec),
            "isNull" => {
                let [arg] = args.as_slice() else {
                    return Err("isNull expects a single property".into());
                };
                let Expr::Property { property } = arg.as_ref() else {
                    return Err("isNull expects a property operand".into());
                };
                let field = (spec.map)(property);
                Ok(json!({"bool": {"must_not": [{"exists": {"field": field}}]}}))
            }
            "s_intersects" | "s_within" | "s_contains" | "s_disjoint" => {
                spatial_clause(op, args, spec)
            }
            other => Err(format!("unsupported cql2 operator '{other}'")),
        },
        _ => Err("unsupported cql2 expression".into()),
    }
}

fn two_args<'a>(
    op: &str,
    args: &'a [Box<cql2::Expr>],
) -> Result<(&'a cql2::Expr, &'a cql2::Expr), String> {
    match args {
        [a, b] => Ok((a, b)),
        _ => Err(format!("'{op}' expects two operands")),
    }
}

fn comparison_clause(op: &str, args: &[Box<cql2::Expr>], spec: FieldSpec) -> Result<Value, String> {
    use cql2::Expr;
    let (a, b) = two_args(op, args)?;
    // The property operand may sit on either side of the literal.
    let (property, lit, flip) = match (a, b) {
        (Expr::Property { property }, lit) => (property, lit, false),
        (lit, Expr::Property { property }) => (property, lit, true),
        _ => return Err(format!("'{op}' expects a property operand")),
    };
    let op = match (op, flip) {
        ("<", true) => ">",
        ("<=", true) => ">=",
        (">", true) => "<",
        (">=", true) => "<=",
        (same, _) => same,
    };
    let field = (spec.map)(property);
    let value = literal_value(lit)?;
    let exact = (spec.exact)(&field, &value);
    Ok(match op {
        "=" => json!({"term": {exact: value}}),
        "<>" => json!({"bool": {"must_not": [{"term": {exact: value}}]}}),
        "<" => json!({"range": {field: {"lt": value}}}),
        "<=" => json!({"range": {field: {"lte": value}}}),
        ">" => json!({"range": {field: {"gt": value}}}),
        ">=" => json!({"range": {field: {"gte": value}}}),
        _ => unreachable!(),
    })
}

fn in_clause(args: &[Box<cql2::Expr>], spec: FieldSpec) -> Result<Value, String> {
    let (a, b) = two_args("in", args)?;
    let (cql2::Expr::Property { property }, cql2::Expr::Array(values)) = (a, b) else {
        return Err("'in' expects a property and an array".into());
    };
    let field = (spec.map)(property);
    let values = values
        .iter()
        .map(|v| literal_value(v))
        .collect::<Result<Vec<_>, _>>()?;
    let field = values
        .first()
        .map(|v| (spec.exact)(&field, v))
        .unwrap_or(field);
    Ok(json!({"terms": {field: values}}))
}

fn between_clause(args: &[Box<cql2::Expr>], spec: FieldSpec) -> Result<Value, String> {
    // cql2-json encodes between as (scalar, [lo, hi]); accept the
    // three-operand spelling too.
    let (prop, lo, hi) = match args {
        [p, range] => match range.as_ref() {
            cql2::Expr::Array(b) if b.len() == 2 => (p.as_ref(), &*b[0], &*b[1]),
            _ => return Err("'between' expects a [lo, hi] array".into()),
        },
        [p, lo, hi] => (p.as_ref(), lo.as_ref(), hi.as_ref()),
        _ => return Err("'between' expects three operands".into()),
    };
    let cql2::Expr::Property { property } = prop else {
        return Err("'between' expects a property first".into());
    };
    let field = (spec.map)(property);
    Ok(json!({"range": {field: {
        "gte": literal_value(lo)?,
        "lte": literal_value(hi)?,
    }}}))
}

fn like_clause(args: &[Box<cql2::Expr>], spec: FieldSpec) -> Result<Value, String> {
    let (a, b) = two_args("like", args)?;
    let (cql2::Expr::Property { property }, cql2::Expr::Literal(pattern)) = (a, b) else {
        return Err("'like' expects a property and a string pattern".into());
    };
    // LIKE wildcards -> wildcard-query wildcards; dynamic string
    // properties live on the `.keyword` sub-field.
    let field = (spec.exact)(&(spec.map)(property), &json!(""));
    let pattern = pattern.replace('%', "*").replace('_', "?");
    Ok(json!({"wildcard": {field: {"value": pattern}}}))
}

fn spatial_clause(op: &str, args: &[Box<cql2::Expr>], spec: FieldSpec) -> Result<Value, String> {
    let (a, b) = two_args(op, args)?;
    let (property, geom) = match (a, b) {
        (cql2::Expr::Property { property }, g) => (property, g),
        (g, cql2::Expr::Property { property }) => (property, g),
        _ => return Err(format!("'{op}' expects a property operand")),
    };
    let field = (spec.map)(property);
    let shape = serde_json::to_value(geom).map_err(|e| e.to_string())?;
    let relation = match op {
        "s_intersects" => "intersects",
        "s_within" => "within",
        "s_contains" => "contains",
        _ => "disjoint",
    };
    Ok(json!({"geo_shape": {field: {"shape": shape, "relation": relation}}}))
}

fn literal_value(expr: &cql2::Expr) -> Result<Value, String> {
    use cql2::Expr;
    match expr {
        // Integral floats emit as integers so term/range JSON stays tidy
        // (exact anyway below 2^53 — covers the big-int search cases).
        Expr::Float(f) if f.fract() == 0.0 && f.abs() <= 9.0e15 => Ok(json!(*f as i64)),
        Expr::Float(f) => Ok(json!(f)),
        Expr::Literal(s) => Ok(json!(s)),
        Expr::Bool(b) => Ok(json!(b)),
        Expr::Timestamp { timestamp } => literal_value(timestamp),
        Expr::Date { date } => literal_value(date),
        Expr::Null => Ok(Value::Null),
        other => Err(format!("unsupported cql2 literal {other:?}")),
    }
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
    fn test_filter_fields_include_single_property() {
        let mut item = serde_json::Map::from_iter([
            ("id".to_string(), json!("i1")),
            ("collection".to_string(), json!("c1")),
            (
                "properties".to_string(),
                json!({"datetime": "d", "other": "x"}),
            ),
            ("links".to_string(), json!([])),
        ]);
        filter_fields(&mut item, &["properties.datetime".to_string()], &[]);
        assert_eq!(
            item["properties"],
            json!({"datetime": "d"}),
            "include keeps only the named leaf"
        );
    }

    #[test]
    fn test_filter_fields_include_miss_returns_identity() {
        let mut item = serde_json::Map::from_iter([
            ("id".to_string(), json!("i1")),
            ("collection".to_string(), json!("c1")),
            ("properties".to_string(), json!({"datetime": "d"})),
        ]);
        filter_fields(&mut item, &["doesnotexist".to_string()], &[]);
        assert_eq!(item.len(), 2);
        assert_eq!(item["id"], "i1");
        assert_eq!(item["collection"], "c1");
    }

    #[test]
    fn test_filter_fields_wildcard_include() {
        let mut item = serde_json::Map::from_iter([(
            "properties".to_string(),
            json!({"proj:centroid": {"lat": 1.0, "lon": 2.0}, "other": "x"}),
        )]);
        filter_fields(&mut item, &["properties.*.lat".to_string()], &[]);
        assert_eq!(item["properties"], json!({"proj:centroid": {"lat": 1.0}}));
    }

    #[test]
    fn test_filter_fields_exclude_after_include() {
        let mut item = serde_json::Map::from_iter([(
            "properties".to_string(),
            json!({"a": {"lat": 1.0, "x": 1}, "b": {"lat": 2.0}}),
        )]);
        filter_fields(
            &mut item,
            &["properties.*.lat".to_string()],
            &["properties.*.lat".to_string()],
        );
        assert_eq!(
            item.len(),
            0,
            "exclude wins over include — empty parents prune"
        );
    }

    #[test]
    fn test_filter_fields_exclude_whole_object() {
        let mut item = serde_json::Map::from_iter([
            ("id".to_string(), json!("i1")),
            ("properties".to_string(), json!({"datetime": "d"})),
        ]);
        filter_fields(&mut item, &[], &["properties".to_string()]);
        assert!(!item.contains_key("properties"));
        assert_eq!(item["id"], "i1");
    }

    #[test]
    fn test_filter_fields_noop() {
        let mut item = serde_json::Map::from_iter([("id".to_string(), json!("i1"))]);
        filter_fields(&mut item, &[], &[]);
        assert_eq!(item["id"], "i1");
    }

    #[test]
    fn test_segment_match_glob() {
        assert!(segment_match("*", "anything"));
        assert!(segment_match("proj:*", "proj:epsg"));
        assert!(segment_match("*centroid*", "proj:centroid"));
        assert!(!segment_match("lat", "latitude"));
        assert!(segment_match("a*z", "abz"));
        assert!(!segment_match("a*z", "ab"));
    }

    #[test]
    fn test_sort_field_mapping() {
        assert_eq!(item_field("id"), "id");
        assert_eq!(item_field("collection"), "collection");
        assert_eq!(item_field("datetime"), "properties.datetime");
        assert_eq!(item_field("properties.created"), "properties.created");
        assert_eq!(item_field("eo:cloud_cover"), "properties.eo:cloud_cover");
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
    fn test_stacql_operators() {
        let q = serde_json::json!({
            "proj:epsg": {"gt": 1000},
            "platform": {"eq": "landsat"},
            "mission": {"ne": "x"},
            "id": {"in": ["a", "b"]}
        })
        .as_object()
        .unwrap()
        .clone();
        let clauses = stacql_clauses(&q).unwrap();
        assert!(clauses.contains(&json!({"range": {"properties.proj:epsg": {"gt": 1000}}})));
        // String properties compare on the dynamic `.keyword` sub-field
        assert!(clauses.contains(&json!({"term": {"properties.platform.keyword": "landsat"}})));
        assert!(clauses.contains(
            &json!({"bool": {"must_not": [{"term": {"properties.mission.keyword": "x"}}]}})
        ));
        // `id` is a top-level item field — not forced under properties.*
        assert!(clauses.contains(&json!({"terms": {"id": ["a", "b"]}})));
    }

    #[test]
    fn test_stacql_invalid() {
        // operator expressions must be objects
        let q = serde_json::json!({"proj:epsg": 5})
            .as_object()
            .unwrap()
            .clone();
        assert!(stacql_clauses(&q).is_err());
        // unknown operators are a 400, not silently ignored
        let q = serde_json::json!({"proj:epsg": {"approx": 5}})
            .as_object()
            .unwrap()
            .clone();
        assert!(stacql_clauses(&q).is_err());
    }

    #[test]
    fn test_cql2_comparisons_and_logic() {
        use stac_api::Filter;
        let filter = Filter::Cql2Json(
            serde_json::json!({
                "op": "and", "args": [
                    {"op": "=", "args": [{"property": "properties.eo:x"}, 42]},
                    {"op": "or", "args": [
                        {"op": ">", "args": [{"property": "eo:y"}, 1]},
                        {"op": "<>", "args": [{"property": "properties.eo:z"}, "v"]}
                    ]}
                ]
            })
            .as_object()
            .unwrap()
            .clone(),
        );
        let clause = cql2_clause(&filter).unwrap();
        let must = clause["bool"]["filter"].as_array().unwrap();
        assert_eq!(must[0], json!({"term": {"properties.eo:x": 42}}));
        let should = must[1]["bool"]["should"].as_array().unwrap();
        assert_eq!(should[0], json!({"range": {"properties.eo:y": {"gt": 1}}}));
        assert_eq!(
            should[1],
            json!({"bool": {"must_not": [{"term": {"properties.eo:z.keyword": "v"}}]}})
        );
    }

    #[test]
    fn test_cql2_between_in_like_isnull() {
        use stac_api::Filter;
        let clause = |v: serde_json::Value| {
            cql2_clause(&Filter::Cql2Json(v.as_object().unwrap().clone())).unwrap()
        };
        assert_eq!(
            clause(json!({"op": "between", "args": [{"property": "eo:x"}, [1, 5]]})),
            json!({"range": {"properties.eo:x": {"gte": 1, "lte": 5}}})
        );
        assert_eq!(
            clause(json!({"op": "in", "args": [{"property": "collection"}, ["a", "b"]]})),
            json!({"terms": {"collection": ["a", "b"]}})
        );
        assert_eq!(
            clause(json!({"op": "like", "args": [{"property": "platform"}, "sent%"]})),
            json!({"wildcard": {"properties.platform.keyword": {"value": "sent*"}}})
        );
        assert_eq!(
            clause(json!({"op": "isNull", "args": [{"property": "eo:x"}]})),
            json!({"bool": {"must_not": [{"exists": {"field": "properties.eo:x"}}]}})
        );
    }

    #[test]
    fn test_cql2_text_and_spatial() {
        use stac_api::Filter;
        // cql2-text form (GET ?filter=) parses through the same path
        let filter = Filter::Cql2Text("properties.eo:cloud_cover <= 10".to_string());
        assert_eq!(
            cql2_clause(&filter).unwrap(),
            json!({"range": {"properties.eo:cloud_cover": {"lte": 10}}})
        );
        let spatial = Filter::Cql2Json(
            serde_json::json!({
                "op": "s_intersects",
                "args": [{"property": "geometry"}, {
                    "type": "Point", "coordinates": [1.0, 2.0]
                }]
            })
            .as_object()
            .unwrap()
            .clone(),
        );
        let clause = cql2_clause(&spatial).unwrap();
        assert_eq!(clause["geo_shape"]["geometry"]["relation"], "intersects");
        assert_eq!(
            clause["geo_shape"]["geometry"]["shape"]["type"],
            json!("Point")
        );
    }

    #[test]
    fn test_datetime_filter_instant() {
        let q = datetime_filter("2023-06-15T00:00:00Z", true);
        let should = q["bool"]["should"].as_array().unwrap();
        // instant items: exact instant; ranged items: start<=v<=end
        assert_eq!(
            should[0]["range"]["properties.datetime"],
            json!({"gte": "2023-06-15T00:00:00Z", "lte": "2023-06-15T00:00:00Z"})
        );
    }

    #[test]
    fn test_datetime_filter_interval() {
        let q = datetime_filter("2023-06-01/2023-07-01", true);
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
    fn test_datetime_filter_use_datetime_false() {
        // use_datetime=false: the instant branch additionally requires
        // neither start_datetime nor end_datetime to exist — an item
        // carrying a range is filtered on the range alone.
        let q = datetime_filter("2023-06-01/2023-07-01", false);
        let instant = &q["bool"]["should"][0];
        let filters = instant["bool"]["filter"].as_array().unwrap();
        assert_eq!(filters.len(), 3);
        assert_eq!(
            filters[0]["range"]["properties.datetime"],
            json!({"gte": "2023-06-01", "lte": "2023-07-01"})
        );
        assert_eq!(
            filters[1]["bool"]["must_not"]["exists"]["field"],
            "properties.start_datetime"
        );
        // and the ranged branch no longer excludes `datetime`
        let ranged = q["bool"]["should"][1]["bool"]["filter"].as_array().unwrap();
        assert_eq!(ranged.len(), 2);
    }

    #[test]
    fn test_datetime_filter_open_interval() {
        // "2023-06-01/.." -> only a lower bound on both branches
        let q = datetime_filter("2023-06-01/..", true);
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

    #[test]
    fn test_queryable_properties_types_and_prefixes() {
        let mapping = json!({"properties": {
            "id": {"type": "keyword"},
            "properties": {"properties": {
                "datetime": {"type": "date"},
                "eo:cloud_cover": {"type": "double"},
                "eo:bands": {"type": "object", "properties": {
                    "center_wavelength": {"type": "float"},
                    "full": {"type": "boolean"}
                }},
                "private_data": {"type": "keyword"}
            }},
            "assets": {"properties": {
                "thumbnail": {"type": "object", "properties": {
                    "href": {"type": "keyword"}
                }}
            }}
        }});
        let props = queryable_properties(&mapping, &[]);
        assert_eq!(props["id"], json!({"type": "string"}));
        assert_eq!(
            props["datetime"],
            json!({"type": "string", "format": "date-time"})
        );
        assert_eq!(props["eo:cloud_cover"], json!({"type": "number"}));
        assert_eq!(
            props["eo:bands.center_wavelength"],
            json!({"type": "number"})
        );
        assert_eq!(props["eo:bands.full"], json!({"type": "boolean"}));
        assert_eq!(props["thumbnail.href"], json!({"type": "string"}));
        assert!(props.contains_key("private_data"));
    }

    #[test]
    fn test_queryable_properties_exclusions() {
        let mapping = json!({"properties": {
            "properties": {"properties": {
                "private_data": {"type": "keyword"},
                "private": {"type": "object", "properties": {
                    "nested": {"type": "keyword"}
                }},
                "eo:cloud_cover": {"type": "double"}
            }}
        }});
        // exclusion applies with or without the properties. prefix and
        // drops whole subtrees
        let props = queryable_properties(
            &mapping,
            &["private_data".into(), "properties.private".into()],
        );
        assert!(!props.contains_key("private_data"));
        assert!(!props.contains_key("private"));
        assert!(!props.contains_key("private.nested"));
        assert!(props.contains_key("eo:cloud_cover"));
    }
}
