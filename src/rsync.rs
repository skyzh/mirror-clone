use crate::error::Result;
use crate::traits::{SnapshotStorage, SourceStorage};

use crate::{common::Mission, error::Error};

use async_trait::async_trait;
use slog::info;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use std::process::Stdio;

#[derive(Debug)]
pub struct Rsync {
    pub base: String,
    pub debug: bool,
}

fn parse_rsync_output(line: &str) -> Result<(&str, &str, &str, &str, &str)> {
    let (permission, rest) = line.split_once(' ').ok_or(Error::NoneError)?;
    let rest = rest.trim_start();
    let (size, rest) = rest.split_once(' ').ok_or(Error::NoneError)?;
    let rest = rest.trim_start();
    let (date, rest) = rest.split_once(' ').ok_or(Error::NoneError)?;
    let rest = rest.trim_start();
    let (time, file) = rest.split_once(' ').ok_or(Error::NoneError)?;
    Ok((permission, size, date, time, file))
}

#[async_trait]
impl SnapshotStorage<String> for Rsync {
    async fn snapshot(&mut self, mission: Mission) -> Result<Vec<String>> {
        let logger = mission.logger;
        let progress = mission.progress;
        let _client = mission.client;

        info!(logger, "running rsync...");

        let mut cmd = Command::new("rsync");
        cmd.kill_on_drop(true);
        cmd.arg("-r").arg(self.base.clone());
        cmd.stdout(Stdio::piped());

        let mut child = cmd.spawn().expect("failed to spawn command");

        let stdout = child
            .stdout
            .take()
            .expect("child did not have a handle to stdout");

        let mut reader = BufReader::new(stdout).lines();

        let result = tokio::spawn(async {
            let status = child.await.map_err(|err| {
                Error::ProcessError(format!("child process encountered an error: {:?}", err))
            })?;
            Ok::<_, Error>(status)
        });

        let mut snapshot = vec![];
        let mut idx = 0;

        while let Some(line) = reader.next_line().await? {
            progress.inc(1);
            idx += 1;
            if self.debug && idx > 1000 {
                break;
            }

            if let Ok((permission, _, _, _, file)) = parse_rsync_output(&line) {
                progress.set_message(file);
                if permission.starts_with("-rw") {
                    // only clone files
                    snapshot.push(file.to_string());
                }
            }
        }

        progress.set_message("waiting for rsync to exit");

        let status = result.await.unwrap()?;
        if !status.success() {
            return Err(Error::ProcessError(format!("exit code: {:?}", status)));
        }

        progress.finish_with_message("done");

        Ok(snapshot)
    }

    fn info(&self) -> String {
        format!("rsync, {:?}", self)
    }
}

#[async_trait]
impl SourceStorage<String, String> for Rsync {
    async fn get_object(&self, snapshot: String, _mission: &Mission) -> Result<String> {
        Ok(snapshot)
    }
}
