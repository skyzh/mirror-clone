//! PyPI source.
//!
//! Pypi is a source storage which scans PyPI. The snapshot is generated by first
//! scanning the package index, then scanning index of every package. This only takes
//! about 5 minutes on SJTUG server, where we fetch data from TUNA mirrors.
//! A PyPI link may contain checksum in its URL, and when taking snapshot, this source
//! will remove checksums from URL.
//!
//! Pypi supports path snapshot, and TransferURL source object.

use crate::common::{Mission, SnapshotConfig, SnapshotPath, TransferURL};
use crate::error::{Error, Result};
use crate::traits::{SnapshotStorage, SourceStorage};
use crate::utils::bar;

use async_trait::async_trait;
use futures_util::{stream, StreamExt, TryStreamExt};
use regex::Regex;
use slog::{info, warn};
use structopt::StructOpt;

#[derive(Debug, Clone, StructOpt)]
pub struct Pypi {
    /// Base of simple index
    #[structopt(
        long,
        default_value = "https://nanomirrors.tuna.tsinghua.edu.cn/pypi/web/simple",
        help = "Base of simple index"
    )]
    pub simple_base: String,
    /// Base of package base
    #[structopt(
        long,
        default_value = "https://nanomirrors.tuna.tsinghua.edu.cn/pypi/web/packages",
        help = "Base of package index"
    )]
    pub package_base: String,
    /// When debug mode is enabled, only first 1000 packages will be selected.
    /// Please add `--no-delete` parameter on simple diff transfer when enabling
    /// debug mode on a production endpoint.
    #[structopt(long)]
    pub debug: bool,
}

#[async_trait]
impl SnapshotStorage<SnapshotPath> for Pypi {
    async fn snapshot(
        &mut self,
        mission: Mission,
        config: &SnapshotConfig,
    ) -> Result<Vec<SnapshotPath>> {
        let logger = mission.logger;
        let progress = mission.progress;
        let client = mission.client;

        info!(logger, "downloading pypi index...");
        let mut index = client
            .get(&format!("{}/", self.simple_base))
            .send()
            .await?
            .text()
            .await?;
        let matcher = Regex::new(r#"<a.*href="(.*?)".*>(.*?)</a>"#).unwrap();

        info!(logger, "parsing index...");
        if self.debug {
            index = index[..1000].to_string();
        }
        let caps: Vec<(String, String)> = matcher
            .captures_iter(&index)
            .map(|cap| (cap[1].to_string(), cap[2].to_string()))
            .collect();

        info!(logger, "downloading package index...");
        progress.set_length(caps.len() as u64);
        progress.set_style(bar());

        let packages: Result<Vec<Vec<(String, String)>>> =
            stream::iter(caps.into_iter().map(|(url, name)| {
                let client = client.clone();
                let simple_base = self.simple_base.clone();
                let progress = progress.clone();
                let matcher = matcher.clone();
                let logger = logger.clone();

                let func = async move {
                    progress.set_message(&name);
                    let package = client
                        .get(&format!("{}/{}", simple_base, url))
                        .send()
                        .await?
                        .text()
                        .await?;
                    let caps: Vec<(String, String)> = matcher
                        .captures_iter(&package)
                        .map(|cap| {
                            let url = format!("{}/{}{}", simple_base, url, &cap[1]);
                            let parsed = url::Url::parse(&url).unwrap();
                            let cleaned: &str = &parsed[..url::Position::AfterPath];
                            (cleaned.to_string(), cap[2].to_string())
                        })
                        .collect();
                    progress.inc(1);
                    Ok::<Vec<(String, String)>, Error>(caps)
                };
                async move {
                    match func.await {
                        Ok(x) => Ok(x),
                        Err(err) => {
                            warn!(logger, "failed to fetch index {:?}", err);
                            Ok(vec![])
                        }
                    }
                }
            }))
            .buffer_unordered(config.concurrent_resolve)
            .try_collect()
            .await;

        let package_base = if self.package_base.ends_with('/') {
            self.package_base.clone()
        } else {
            format!("{}/", self.package_base)
        };

        let snapshot = packages?
            .into_iter()
            .flatten()
            .filter_map(|(url, _)| {
                if url.starts_with(&package_base) {
                    Some(url[package_base.len()..].to_string())
                } else {
                    warn!(logger, "PyPI package isn't stored on base: {:?}", url);
                    None
                }
            })
            .collect();

        progress.finish_with_message("done");

        Ok(crate::utils::snapshot_string_to_path(snapshot))
    }

    fn info(&self) -> String {
        format!("pypi, {:?}", self)
    }
}

#[async_trait]
impl SourceStorage<SnapshotPath, TransferURL> for Pypi {
    async fn get_object(&self, snapshot: &SnapshotPath, _mission: &Mission) -> Result<TransferURL> {
        Ok(TransferURL(format!("{}/{}", self.package_base, snapshot.0)))
    }
}
