// src/links.rs
use stac::Link;
use url::Url;

pub struct LinkEngine {
    pub base_url: Url,
}

impl LinkEngine {
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: Url::parse(base_url).expect("Invalid base URL"),
        }
    }

    /// Formats links for a Collection accessed via a Scoped Catalog route:
    /// `/catalogs/{catalog_id}/collections/{collection_id}`
    pub fn format_scoped_collection_links(
        &self,
        collection_id: &str,
        scoped_catalog_id: &str,
        all_parent_ids: &[String],
        hide_alternate_parents: bool,
    ) -> Vec<Link> {
        let scoped_self = self
            .base_url
            .join(&format!(
                "/catalogs/{scoped_catalog_id}/collections/{collection_id}"
            ))
            .unwrap();
        let contextual_parent = self
            .base_url
            .join(&format!("/catalogs/{scoped_catalog_id}"))
            .unwrap();
        let canonical_url = self
            .base_url
            .join(&format!("/collections/{collection_id}"))
            .unwrap();

        let mut links = vec![
            Link::self_(scoped_self.to_string()).json(),
            Link::root(self.base_url.to_string()).json(),
            // MUST lock parent link to the contextual path for UI breadcrumbs
            Link::parent(contextual_parent.to_string())
                .json()
                .title(scoped_catalog_id.to_string()),
            Link::new(canonical_url.to_string(), "canonical").json(),
            Link::new(format!("{scoped_self}/items"), "items").geojson(),
        ];

        // Expose alternate poly-hierarchy parents as "related" and "duplicate"
        for parent in all_parent_ids {
            if parent != scoped_catalog_id && !hide_alternate_parents {
                let alt_parent_url = self.base_url.join(&format!("/catalogs/{parent}")).unwrap();
                let dup_scoped_url = self
                    .base_url
                    .join(&format!("/catalogs/{parent}/collections/{collection_id}"))
                    .unwrap();

                links.push(
                    Link::new(alt_parent_url.to_string(), "related")
                        .json()
                        .title(parent.clone()),
                );
                links.push(
                    Link::new(dup_scoped_url.to_string(), "duplicate")
                        .json()
                        .title(format!("Collection in catalog: {parent}")),
                );
            }
        }

        links
    }

    /// Formats links for the Sub-Catalog landing page: `/catalogs/{catalog_id}`
    pub fn format_sub_catalog_links(
        &self,
        catalog_id: &str,
        primary_parent_id: Option<&str>,
        alt_parent_ids: &[String],
    ) -> Vec<Link> {
        let self_url = self
            .base_url
            .join(&format!("/catalogs/{catalog_id}"))
            .unwrap();
        let parent_url = match primary_parent_id {
            Some(pid) if pid != "root" => self.base_url.join(&format!("/catalogs/{pid}")).unwrap(),
            _ => self.base_url.clone(), // Root catalog
        };

        let mut links = vec![
            Link::self_(self_url.to_string()).json(),
            Link::root(self.base_url.to_string()).json(),
            Link::parent(parent_url.to_string()).json(),
            Link::new(format!("{self_url}/collections"), "data").json(),
            Link::new(format!("{self_url}/children"), "children").json(),
            Link::new(format!("{self_url}/search"), "search").geojson(),
        ];

        for alt_parent in alt_parent_ids {
            let alt_url = self
                .base_url
                .join(&format!("/catalogs/{alt_parent}"))
                .unwrap();
            links.push(
                Link::new(alt_url.to_string(), "related")
                    .json()
                    .title(format!("Alternate parent: {alt_parent}")),
            );
        }

        links
    }
}
