//! STAC API request/response data structures.
//!
//! Vendored from `stac-api` 0.8.2 (MIT/Apache-2.0, stac-utils/rustac), trimmed
//! to the server-side surface this crate uses: request deserialization plus
//! `valid()` checks and the `GetSearch` -> `Search` conversion. The crate's
//! client, URL builder, and in-memory `matches()` helpers (a server concern we
//! delegate to OpenSearch) are not vendored. `stac-api` 0.8.2 pins `stac`
//! 0.14; owning these types lets us track current `stac` releases.
//!
//! Error channel is `String` — call sites map to `ApiError::BadRequest` /
//! `ApiError::Internal` with the same messages the crate produced.

use cql2::Expr;
use geojson::Geometry;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use stac::{Bbox, Link};
use std::convert::Infallible;
use std::fmt::{Display, Formatter};
use std::ops::{Deref, DerefMut};
use std::str::FromStr;

/// A STAC API Item: a raw object, since the fields extension may strip any
/// member and the result may no longer be a schema-conformant `stac::Item`.
pub type ApiItem = Map<String, Value>;

/// The language of the filter expression.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "filter-lang", content = "filter")]
pub enum Filter {
    /// `cql2-text`
    #[serde(rename = "cql2-text")]
    Cql2Text(String),

    /// `cql2-json`
    #[serde(rename = "cql2-json")]
    Cql2Json(Map<String, Value>),
}

impl Default for Filter {
    fn default() -> Self {
        Filter::Cql2Json(Default::default())
    }
}

impl FromStr for Filter {
    type Err = Infallible;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Filter::Cql2Text(s.to_string()))
    }
}

impl TryFrom<Filter> for Expr {
    type Error = String;
    fn try_from(value: Filter) -> Result<Self, Self::Error> {
        match value {
            Filter::Cql2Json(json) => {
                serde_json::from_value(Value::Object(json)).map_err(|e| e.to_string())
            }
            Filter::Cql2Text(text) => cql2::parse_text(&text).map_err(|e| e.to_string()),
        }
    }
}

/// Include/exclude fields from item collections (fields extension).
#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq)]
pub struct Fields {
    /// Fields to include.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,

    /// Fields to exclude.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
}

impl FromStr for Fields {
    type Err = Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut include = Vec::new();
        let mut exclude = Vec::new();
        for field in s.split(',').filter(|s| !s.is_empty()) {
            if let Some(field) = field.strip_prefix('-') {
                exclude.push(field.to_string());
            } else if let Some(field) = field.strip_prefix('+') {
                include.push(field.to_string());
            } else {
                include.push(field.to_string());
            }
        }
        Ok(Fields { include, exclude })
    }
}

impl Display for Fields {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let mut fields = Vec::new();
        for include in &self.include {
            fields.push(include.to_string());
        }
        for exclude in &self.exclude {
            fields.push(format!("-{exclude}"));
        }
        write!(f, "{}", fields.join(","))
    }
}

/// Fields by which to sort results.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Sortby {
    /// The field to sort by.
    pub field: String,

    /// The direction to sort by.
    pub direction: Direction,
}

/// The direction of sorting.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub enum Direction {
    /// Ascending
    #[serde(rename = "asc")]
    Ascending,

    /// Descending
    #[serde(rename = "desc")]
    Descending,
}

impl Sortby {
    /// Creates a new ascending sortby for the field.
    pub fn asc(field: impl ToString) -> Sortby {
        Sortby {
            field: field.to_string(),
            direction: Direction::Ascending,
        }
    }

    /// Creates a new descending sortby for the field.
    pub fn desc(field: impl ToString) -> Sortby {
        Sortby {
            field: field.to_string(),
            direction: Direction::Descending,
        }
    }
}

impl FromStr for Sortby {
    type Err = Infallible;

    fn from_str(s: &str) -> Result<Self, Infallible> {
        if let Some(s) = s.strip_prefix('+') {
            Ok(Sortby::asc(s))
        } else if let Some(s) = s.strip_prefix('-') {
            Ok(Sortby::desc(s))
        } else {
            Ok(Sortby::asc(s))
        }
    }
}

impl Display for Sortby {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self.direction {
            Direction::Ascending => write!(f, "{}", self.field),
            Direction::Descending => write!(f, "-{}", self.field),
        }
    }
}

/// Parameters for the items endpoint from STAC API - Features.
#[derive(Clone, Default, Debug, Serialize, Deserialize)]
pub struct Items {
    /// The maximum number of results to return (page size).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,

    /// Requested bounding box.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bbox: Option<Bbox>,

    /// Single date+time, or a range ('/' separator), formatted to RFC 3339.
    ///
    /// Use double dots `..` for open date ranges.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub datetime: Option<String>,

    /// Include/exclude fields from item collections.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fields: Option<Fields>,

    /// Fields by which to sort results.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub sortby: Vec<Sortby>,

    /// Server must only accept CRS84 as a valid value.
    #[serde(skip_serializing_if = "Option::is_none", rename = "filter-crs")]
    pub filter_crs: Option<String>,

    /// CQL2 filter expression.
    #[serde(skip_serializing_if = "Option::is_none", flatten)]
    pub filter: Option<Filter>,

    /// Additional filtering based on properties.
    ///
    /// It is recommended to use the filter extension instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<Map<String, Value>>,

    /// Additional fields.
    #[serde(flatten)]
    pub additional_fields: Map<String, Value>,
}

/// GET parameters for the items endpoint from STAC API - Features.
#[derive(Clone, Default, Debug, Serialize, Deserialize)]
pub struct GetItems {
    /// The maximum number of results to return (page size).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<String>,

    /// Requested bounding box.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bbox: Option<String>,

    /// Single date+time, or a range ('/' separator), formatted to RFC 3339.
    ///
    /// Use double dots `..` for open date ranges.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub datetime: Option<String>,

    /// Include/exclude fields from item collections.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fields: Option<String>,

    /// Fields by which to sort results.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sortby: Option<String>,

    /// Server must only accept CRS84 as a valid value.
    #[serde(skip_serializing_if = "Option::is_none", rename = "filter-crs")]
    pub filter_crs: Option<String>,

    /// This should always be cql2-text if present.
    #[serde(skip_serializing_if = "Option::is_none", rename = "filter-lang")]
    pub filter_lang: Option<String>,

    /// CQL2 filter expression.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,

    /// Additional fields.
    #[serde(flatten)]
    pub additional_fields: indexmap::IndexMap<String, String>,
}

impl Items {
    /// Runs a set of validity checks on this query and returns an error if it
    /// is invalid.
    pub fn valid(self) -> Result<Items, String> {
        if let Some(bbox) = self.bbox.as_ref() {
            if !bbox.is_valid() {
                return Err(format!("invalid bbox ({bbox:?})"));
            }
        }
        if let Some(datetime) = self.datetime.as_deref() {
            if let Some((start, end)) = datetime.split_once('/') {
                let (start, end) = (
                    maybe_parse_from_rfc3339(start)?,
                    maybe_parse_from_rfc3339(end)?,
                );
                if let Some(start) = start {
                    if let Some(end) = end {
                        if end < start {
                            return Err(format!("start ({start}) is after end ({end})"));
                        }
                    }
                } else if end.is_none() {
                    return Err("empty datetime interval".to_string());
                }
            } else {
                let _ = maybe_parse_from_rfc3339(datetime)?;
            }
        }
        Ok(self)
    }
}

impl TryFrom<GetItems> for Items {
    type Error = String;

    fn try_from(get_items: GetItems) -> Result<Items, Self::Error> {
        let bbox = if let Some(value) = get_items.bbox {
            let mut bbox = Vec::new();
            for s in value.split(',') {
                bbox.push(
                    s.parse()
                        .map_err(|e: std::num::ParseFloatError| e.to_string())?,
                )
            }
            Some(bbox.try_into().map_err(|_| "invalid bbox".to_string())?)
        } else {
            None
        };

        let sortby = get_items
            .sortby
            .map(|s| {
                let mut sortby = Vec::new();
                for s in s.split(',') {
                    sortby.push(s.parse().expect("infallible"));
                }
                sortby
            })
            .unwrap_or_default();

        Ok(Items {
            limit: get_items
                .limit
                .map(|limit| limit.parse())
                .transpose()
                .map_err(|e: std::num::ParseIntError| e.to_string())?,
            bbox,
            datetime: get_items.datetime,
            fields: get_items
                .fields
                .map(|fields| fields.parse().expect("infallible")),
            sortby,
            filter_crs: get_items.filter_crs,
            filter: get_items.filter.map(Filter::Cql2Text),
            query: None,
            additional_fields: get_items
                .additional_fields
                .into_iter()
                .map(|(key, value)| (key, Value::String(value)))
                .collect(),
        })
    }
}

/// The core parameters for STAC search are defined by OAFeat, and STAC adds a
/// few parameters for convenience.
#[derive(Clone, Default, Debug, Serialize, Deserialize)]
pub struct Search {
    /// Many fields are shared with [Items], so we re-use that structure.
    #[serde(flatten)]
    pub items: Items,

    /// Searches items by performing intersection between their geometry and
    /// provided GeoJSON geometry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intersects: Option<Geometry>,

    /// Array of Item ids to return.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub ids: Vec<String>,

    /// Array of one or more Collection IDs that each matching Item must be in.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub collections: Vec<String>,
}

/// GET parameters for the item search endpoint.
#[derive(Clone, Default, Debug, Serialize, Deserialize)]
pub struct GetSearch {
    /// Many fields are shared with [GetItems], so we re-use that structure.
    #[serde(flatten)]
    pub items: GetItems,

    /// Searches items by performing intersection between their geometry and
    /// provided GeoJSON geometry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intersects: Option<String>,

    /// Comma-delimited list of Item ids to return.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ids: Option<String>,

    /// Comma-delimited list of one or more Collection IDs that each matching
    /// Item must be in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collections: Option<String>,
}

impl Search {
    /// Creates a new, empty search.
    pub fn new() -> Search {
        Search::default()
    }

    /// Returns an error if this search is invalid, e.g. if both bbox and
    /// intersects are specified.
    pub fn valid(mut self) -> Result<Search, String> {
        self.items = self.items.valid()?;
        if self.items.bbox.is_some() & self.intersects.is_some() {
            Err("search has bbox and intersects".to_string())
        } else {
            Ok(self)
        }
    }
}

impl TryFrom<GetSearch> for Search {
    type Error = String;

    fn try_from(get_search: GetSearch) -> Result<Search, Self::Error> {
        let items: Items = get_search.items.try_into()?;
        let intersects = get_search
            .intersects
            .map(|intersects| serde_json::from_str(&intersects))
            .transpose()
            .map_err(|e: serde_json::Error| e.to_string())?;
        let collections = get_search
            .collections
            .map(|collections| collections.split(',').map(|s| s.to_string()).collect())
            .unwrap_or_default();
        let ids = get_search
            .ids
            .map(|ids| ids.split(',').map(|s| s.to_string()).collect())
            .unwrap_or_default();
        Ok(Search {
            items,
            intersects,
            ids,
            collections,
        })
    }
}

impl From<Items> for Search {
    fn from(items: Items) -> Self {
        Search {
            items,
            ..Default::default()
        }
    }
}

impl Deref for Search {
    type Target = Items;
    fn deref(&self) -> &Self::Target {
        &self.items
    }
}

impl DerefMut for Search {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.items
    }
}

const ITEM_COLLECTION_TYPE: &str = "FeatureCollection";

fn item_collection_type() -> String {
    ITEM_COLLECTION_TYPE.to_string()
}

fn deserialize_item_collection_type<'de, D>(
    deserializer: D,
) -> std::result::Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let r#type = String::deserialize(deserializer)?;
    if r#type != ITEM_COLLECTION_TYPE {
        Err(serde::de::Error::invalid_value(
            serde::de::Unexpected::Str(&r#type),
            &ITEM_COLLECTION_TYPE,
        ))
    } else {
        Ok(r#type)
    }
}

/// The return value of the `/items` and `/search` endpoints.
///
/// Defined by the [itemcollection
/// fragment](https://github.com/radiantearth/stac-api-spec/blob/main/fragments/itemcollection/README.md).
#[derive(Debug, Serialize, Deserialize, Default)]
pub struct ItemCollection {
    #[serde(
        default = "item_collection_type",
        deserialize_with = "deserialize_item_collection_type"
    )]
    r#type: String,

    /// A possibly-empty array of Item objects.
    #[serde(rename = "features")]
    pub items: Vec<ApiItem>,

    /// An array of Links related to this ItemCollection.
    pub links: Vec<Link>,

    /// The number of Items that meet the selection parameters, possibly
    /// estimated.
    #[serde(skip_serializing_if = "Option::is_none", rename = "numberMatched")]
    pub number_matched: Option<u64>,

    /// The number of Items in the features array.
    #[serde(skip_serializing_if = "Option::is_none", rename = "numberReturned")]
    pub number_returned: Option<u64>,

    /// The search-related metadata for the [ItemCollection].
    ///
    /// Part of the [context extension](https://github.com/stac-api-extensions/context).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<Context>,

    /// Additional fields.
    #[serde(flatten)]
    pub additional_fields: Map<String, Value>,

    /// Optional pagination information for the next page (not serialized).
    #[serde(skip)]
    pub next: Option<Map<String, Value>>,

    /// Optional pagination information for the previous page (not serialized).
    #[serde(skip)]
    pub prev: Option<Map<String, Value>>,

    /// Optional pagination information for the first page (not serialized).
    #[serde(skip)]
    pub first: Option<Map<String, Value>>,

    /// Optional pagination information for the last page (not serialized).
    #[serde(skip)]
    pub last: Option<Map<String, Value>>,
}

/// The search-related metadata for the [ItemCollection].
///
/// Part of the [context extension](https://github.com/stac-api-extensions/context).
#[derive(Debug, Serialize, Deserialize)]
pub struct Context {
    /// The count of results returned by this response.
    pub returned: u64,

    /// The maximum number of results to which the result was limited.
    pub limit: Option<u64>,

    /// The count of total number of results that match for this query,
    /// possibly estimated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched: Option<u64>,

    /// Additional fields.
    #[serde(flatten)]
    pub additional_fields: Map<String, Value>,
}

impl ItemCollection {
    /// Creates a new [ItemCollection] from a vector of items.
    pub fn new(items: Vec<ApiItem>) -> Result<ItemCollection, String> {
        Ok(ItemCollection {
            r#type: item_collection_type(),
            number_returned: Some(items.len() as u64),
            items,
            ..Default::default()
        })
    }
}

impl From<Vec<ApiItem>> for ItemCollection {
    fn from(items: Vec<ApiItem>) -> Self {
        ItemCollection {
            r#type: item_collection_type(),
            items,
            ..Default::default()
        }
    }
}

fn maybe_parse_from_rfc3339(
    s: &str,
) -> Result<Option<chrono::DateTime<chrono::FixedOffset>>, String> {
    if s.is_empty() || s == ".." {
        Ok(None)
    } else {
        chrono::DateTime::parse_from_rfc3339(s)
            .map(Some)
            .map_err(|e| e.to_string())
    }
}
