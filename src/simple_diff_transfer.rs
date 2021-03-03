use indicatif::{MultiProgress, ProgressBar};
use reqwest::ClientBuilder;

use crate::common::{Mission, SnapshotConfig, SnapshotPath, TransferPath};
use crate::error::{Error, Result};
use crate::timeout::{TryTimeoutExt, TryTimeoutFutureExt};
use crate::traits::{SnapshotStorage, SourceStorage, TargetStorage};
use crate::utils::{create_logger, spinner};

use futures_util::StreamExt;
use rand::prelude::*;
use slog::{debug, info, o, warn};

use std::sync::Arc;
use std::time::Duration;

#[derive(Debug)]
pub struct SimpleDiffTransferConfig {
    pub progress: bool,
    pub snapshot_config: SnapshotConfig,
}

pub struct SimpleDiffTransfer<Source, Target, Item>
where
    Source: SourceStorage<SnapshotPath, Item> + SnapshotStorage<SnapshotPath>,
    Target: TargetStorage<SnapshotPath, Item> + SnapshotStorage<SnapshotPath>,
{
    source: Source,
    target: Target,
    config: SimpleDiffTransferConfig,
    _phantom: std::marker::PhantomData<Item>,
}

impl<Source, Target, Item> SimpleDiffTransfer<Source, Target, Item>
where
    Source: SourceStorage<SnapshotPath, Item> + SnapshotStorage<SnapshotPath>,
    Target: TargetStorage<SnapshotPath, Item> + SnapshotStorage<SnapshotPath>,
{
    pub fn new(source: Source, target: Target, config: SimpleDiffTransferConfig) -> Self {
        Self {
            source,
            target,
            config,
            _phantom: std::marker::PhantomData,
        }
    }

    fn debug_snapshot(logger: slog::Logger, snapshot: &[SnapshotPath]) {
        let selected: Vec<_> = snapshot
            .choose_multiple(&mut rand::thread_rng(), 50)
            .collect();
        for item in selected {
            debug!(logger, "{}", item.0);
        }
    }

    pub async fn transfer(mut self) -> Result<()> {
        let logger = create_logger();
        let client = ClientBuilder::new()
            .user_agent(format!(
                "mirror-clone / 0.1 ({})",
                std::env::var("MIRROR_CLONE_SITE").unwrap_or("mirror.sjtu.edu.cn".to_string())
            ))
            .connect_timeout(Duration::from_secs(10))
            .build()?;
        info!(logger, "using simple diff transfer"; "config" => format!("{:?}", self.config));
        info!(logger, "begin transfer"; "source" => self.source.info(), "target" => self.target.info());

        info!(logger, "taking snapshot...");

        let all_progress = MultiProgress::new();
        let source_progress = all_progress.add(ProgressBar::new(0));
        source_progress.set_style(spinner());
        source_progress.set_prefix("[source]");
        let target_progress = all_progress.add(ProgressBar::new(0));
        target_progress.set_style(spinner());
        target_progress.set_prefix("[target]");

        let source_mission = Mission {
            client: client.clone(),
            progress: source_progress,
            logger: logger.new(o!("task" => "snapshot.source")),
        };

        let target_mission = Mission {
            client: client.clone(),
            progress: target_progress,
            logger: logger.new(o!("task" => "snapshot.target")),
        };

        let config_progress = self.config.progress;
        let (source_snapshot, target_snapshot, _) = tokio::join!(
            self.source
                .snapshot(source_mission, &self.config.snapshot_config),
            self.target
                .snapshot(target_mission, &self.config.snapshot_config),
            tokio::task::spawn_blocking(move || {
                if config_progress {
                    all_progress.join().unwrap()
                }
            })
        );

        let source_snapshot = source_snapshot?;
        let target_snapshot = target_snapshot?;

        info!(
            logger,
            "source {} objects, target {} objects",
            source_snapshot.len(),
            target_snapshot.len()
        );

        Self::debug_snapshot(logger.clone(), &source_snapshot);
        Self::debug_snapshot(logger.clone(), &target_snapshot);

        info!(logger, "mirror in progress...");

        let progress = if self.config.progress {
            ProgressBar::new(source_snapshot.len() as u64)
        } else {
            ProgressBar::hidden()
        };
        progress.set_style(crate::utils::bar());
        progress.set_prefix("mirror");

        let source_mission = Arc::new(Mission {
            client: client.clone(),
            progress: ProgressBar::hidden(),
            logger: logger.new(o!("task" => "mirror.source")),
        });

        let target_mission = Arc::new(Mission {
            client: client.clone(),
            progress: ProgressBar::hidden(),
            logger: logger.new(o!("task" => "mirror.target")),
        });

        info!(logger, "generating transfer plan...");

        let source_sort = tokio::task::spawn_blocking(move || {
            let mut source_snapshot: Vec<SnapshotPath> = source_snapshot;
            source_snapshot.sort();
            source_snapshot
        });

        let target_sort = tokio::task::spawn_blocking(move || {
            let mut target_snapshot: Vec<SnapshotPath> = target_snapshot;
            target_snapshot.sort();
            target_snapshot
        });

        let (source_snapshot, target_snapshot) = tokio::join!(source_sort, target_sort);

        let source_snapshot = source_snapshot
            .map_err(|err| Error::ProcessError(format!("error while sorting: {:?}", err)))?;
        let target_snapshot = target_snapshot
            .map_err(|err| Error::ProcessError(format!("error while sorting: {:?}", err)))?;

        let source = Arc::new(self.source);
        let target = Arc::new(self.target);

        let map_snapshot = |source_snapshot: SnapshotPath| {
            progress.set_message(&source_snapshot.0);
            let source = source.clone();
            let target = target.clone();
            let source_mission = source_mission.clone();
            let target_mission = target_mission.clone();
            let logger = logger.clone();

            let func = async move {
                let source_object = source
                    .get_object(&source_snapshot, &source_mission)
                    .timeout(Duration::from_secs(60))
                    .await
                    .into_result()?;
                if let Err(err) = target
                    .put_object(&source_snapshot, source_object, &target_mission)
                    .timeout(Duration::from_secs(60))
                    .await
                    .into_result()
                {
                    warn!(target_mission.logger, "error while transfer: {:?}", err);
                }
                Ok::<(), Error>(())
            };

            async move {
                if let Err(err) = func.await {
                    warn!(logger, "failed to fetch index {:?}", err);
                }
            }
        };

        let mut results = futures::stream::iter(source_snapshot.into_iter().map(map_snapshot))
            .buffer_unordered(128);

        while let Some(_x) = results.next().await {
            progress.inc(1);
        }

        info!(logger, "transfer complete");

        Ok(())
    }
}
