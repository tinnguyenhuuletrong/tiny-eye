//! Object storage (local disk or S3) and the key layout of SPEC §4.1.
//!
//! Every node writes under its own prefix `<node.region>/<node.id>/` of `storage.url`, so nodes
//! sharing one bucket (a fleet) never collide:
//!
//!   <region>/<id>/<table>/tenant=<t>/date=<d>/hour=<h>/<level>-<ulid>-<node>.parquet   data
//!   <region>/<id>/commits/<epoch>-<seq>.json                                          envelopes
//!   <region>/<id>/lake/                                                               DuckLake data path
//!   <region>/<id>/_fleet/catalog.ducklake.gz                                          published catalog copy
//!   _fleet/<id>.json                                                                  fleet manifest (shared root)

use crate::config::Config;
use anyhow::{Context, Result};
use bytes::Bytes;
use futures::TryStreamExt;
use object_store::aws::{AmazonS3Builder, S3ConditionalPut};
use object_store::local::LocalFileSystem;
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt, PutMode, PutOptions, PutPayload};
use std::sync::Arc;

#[derive(Clone)]
pub struct Store {
    pub inner: Arc<dyn ObjectStore>,
    /// Absolute URL prefix DuckDB uses to read the same objects (`/abs/dir` or `s3://bucket/prefix`).
    pub url_prefix: String,
    /// Prefix inside the bucket (S3) — keys are relative to it.
    prefix: String,
}

impl Store {
    pub fn from_config(cfg: &Config) -> Result<Store> {
        let url = cfg.storage.url.trim_end_matches('/');
        if let Some(rest) = url.strip_prefix("s3://") {
            let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
            let s3 = &cfg.storage.s3;
            let mut b = AmazonS3Builder::new()
                .with_bucket_name(bucket)
                .with_region(&s3.region)
                .with_allow_http(s3.allow_http)
                // If-None-Match: a retried flush can never overwrite an applied envelope (E1).
                .with_conditional_put(S3ConditionalPut::ETagMatch);
            if !s3.endpoint.is_empty() {
                b = b.with_endpoint(&s3.endpoint).with_virtual_hosted_style_request(false);
            }
            if !s3.access_key_id.is_empty() {
                b = b.with_access_key_id(&s3.access_key_id).with_secret_access_key(&s3.secret_access_key);
            }
            let store = b.build().with_context(|| format!("S3 client for {url}"))?;
            Ok(Store { inner: Arc::new(store), url_prefix: url.to_string(), prefix: prefix.trim_matches('/').to_string() })
        } else {
            let dir = url.strip_prefix("file://").unwrap_or(url);
            std::fs::create_dir_all(dir).with_context(|| format!("creating {dir}"))?;
            let abs = std::fs::canonicalize(dir)?;
            let store = LocalFileSystem::new_with_prefix(&abs)?.with_automatic_cleanup(true);
            Ok(Store { inner: Arc::new(store), url_prefix: abs.display().to_string(), prefix: String::new() })
        }
    }

    /// The same storage seen from `sub/` (a node's own prefix `<region>/<id>` inside the root that
    /// every node of a fleet shares).
    pub fn scoped(&self, sub: &str) -> Store {
        let sub = sub.trim_matches('/');
        Store {
            inner: self.inner.clone(),
            url_prefix: format!("{}/{sub}", self.url_prefix),
            prefix: if self.prefix.is_empty() { sub.to_string() } else { format!("{}/{sub}", self.prefix) },
        }
    }

    fn path(&self, key: &str) -> Path {
        if self.prefix.is_empty() { Path::from(key) } else { Path::from(format!("{}/{key}", self.prefix)) }
    }

    /// The URL DuckDB reads for `key`.
    pub fn url(&self, key: &str) -> String {
        format!("{}/{key}", self.url_prefix)
    }

    /// `key` for a URL produced by [`Store::url`], if it is ours.
    pub fn key_of(&self, url: &str) -> Option<String> {
        url.strip_prefix(&self.url_prefix).map(|k| k.trim_start_matches('/').to_string())
    }

    pub async fn put(&self, key: &str, data: Bytes) -> Result<()> {
        self.inner.put(&self.path(key), PutPayload::from(data)).await.with_context(|| format!("PUT {key}"))?;
        Ok(())
    }

    /// Create-only PUT (`If-None-Match: *`): `true` if this call created the object, `false`
    /// if it already existed (left untouched).
    pub async fn put_new(&self, key: &str, data: Bytes) -> Result<bool> {
        let opts = PutOptions { mode: PutMode::Create, ..Default::default() };
        match self.inner.put_opts(&self.path(key), PutPayload::from(data), opts).await {
            Ok(_) => Ok(true),
            Err(object_store::Error::AlreadyExists { .. }) | Err(object_store::Error::Precondition { .. }) => Ok(false),
            Err(e) => Err(e).with_context(|| format!("PUT (create) {key}")),
        }
    }

    pub async fn get(&self, key: &str) -> Result<Bytes> {
        Ok(self.inner.get(&self.path(key)).await.with_context(|| format!("GET {key}"))?.bytes().await?)
    }

    /// GET with the object's last-modified time (unix ms), unless the object is larger than
    /// `max` bytes (`None`; the body is not read).
    pub async fn get_at_most(&self, key: &str, max: u64) -> Result<Option<(Bytes, i64)>> {
        let r = self.inner.get(&self.path(key)).await.with_context(|| format!("GET {key}"))?;
        if r.meta.size > max {
            return Ok(None);
        }
        let modified = r.meta.last_modified.timestamp_millis();
        Ok(Some((r.bytes().await.with_context(|| format!("GET {key}"))?, modified)))
    }

    pub async fn delete(&self, key: &str) -> Result<()> {
        match self.inner.delete(&self.path(key)).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(e).with_context(|| format!("DELETE {key}")),
        }
    }

    /// Keys under `prefix`, relative to the store root.
    pub async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        Ok(self.list_modified(prefix).await?.into_iter().map(|(k, _)| k).collect())
    }

    /// Like [`Store::list`], with each object's last-modified time (unix ms) from the listing.
    pub async fn list_modified(&self, prefix: &str) -> Result<Vec<(String, i64)>> {
        let p = self.path(prefix);
        let items: Vec<_> = self.inner.list(Some(&p)).try_collect().await.with_context(|| format!("LIST {prefix}"))?;
        let strip = if self.prefix.is_empty() { String::new() } else { format!("{}/", self.prefix) };
        Ok(items
            .into_iter()
            .map(|m| (m.location.to_string().trim_start_matches(&strip).to_string(), m.last_modified.timestamp_millis()))
            .collect())
    }
}

/// `YYYY-MM-DD` of a unix hour number (civil-from-days, no calendar dependency).
pub fn date_of_hour(hour: i64) -> String {
    let z = hour.div_euclid(24) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// `<table>/tenant=<t>/date=<d>/hour=<h>/<level>-<ulid>-<node>.parquet` (SPEC §4.1), relative to
/// the node's own prefix.
pub fn data_key(table: &str, tenant: &str, hour: i64, level: &str, node: &str) -> String {
    format!(
        "{table}/tenant={tenant}/date={}/hour={:02}/{level}-{}-{node}.parquet",
        date_of_hour(hour),
        hour.rem_euclid(24),
        ulid::Ulid::generate()
    )
}

/// Unix hour of a data key, parsed back from `date=` and `hour=` (compaction / retention).
pub fn hour_of_key(key: &str) -> Option<i64> {
    let date = key.split("/date=").nth(1)?.get(..10)?;
    let hh: i64 = key.split("/hour=").nth(1)?.get(..2)?.parse().ok()?;
    let mut parts = date.split('-').map(|p| p.parse::<i64>().ok());
    let (y, m, d) = (parts.next()??, parts.next()??, parts.next()??);
    // days-from-civil
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 24 + hh)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn scoped_store_writes_under_its_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = format!("storage: {{ url: {} }}\n", dir.path().display());
        let root = Store::from_config(&Config::from_layers(Some(&yaml), &[]).unwrap()).unwrap();
        let node = root.scoped("eu/n1");
        node.put("commits/x.json", Bytes::from_static(b"{}")).await.unwrap();
        assert!(dir.path().join("eu/n1/commits/x.json").is_file());
        assert_eq!(node.list("commits/").await.unwrap(), vec!["commits/x.json".to_string()]);
        assert_eq!(root.list("eu/").await.unwrap(), vec!["eu/n1/commits/x.json".to_string()]);
        assert_eq!(node.url("k"), format!("{}/eu/n1/k", root.url_prefix));
        assert_eq!(node.key_of(&node.url("a/b")).as_deref(), Some("a/b"));
    }

    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn get_at_most_skips_larger_objects_without_reading_them() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = format!("storage: {{ url: {} }}\n", dir.path().display());
        let s = Store::from_config(&Config::from_layers(Some(&yaml), &[]).unwrap()).unwrap();
        s.put("k", Bytes::from(vec![7u8; 100])).await.unwrap();
        assert!(s.get_at_most("k", 99).await.unwrap().is_none(), "one byte over");
        let (b, modified) = s.get_at_most("k", 100).await.unwrap().expect("exactly the limit");
        assert_eq!(b.len(), 100);
        assert!((modified - crate::node::now_ms()).abs() < 60_000, "last-modified from the store: {modified}");
    }

    #[test]
    fn hour_round_trips_through_key() {
        for hour in [0i64, 489_650, 489_650 + 23, 500_000, 1_000_000] {
            let k = data_key("spans", "default", hour, "l0", "n0");
            assert_eq!(hour_of_key(&k), Some(hour), "{k}");
        }
        assert_eq!(date_of_hour(1_762_740_000_000 / 3_600_000), "2025-11-10");
    }
}
