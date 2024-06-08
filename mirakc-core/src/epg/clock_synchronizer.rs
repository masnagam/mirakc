use std::collections::HashMap;
use std::sync::Arc;

use actlet::prelude::*;
use serde::Deserialize;
use tokio::io::AsyncReadExt;

#[cfg(test)]
use serde::Serialize;
use tracing::Instrument;

use crate::command_util;
use crate::config::ChannelConfig;
use crate::config::Config;
use crate::epg::*;
use crate::models::*;
use crate::tuner::*;

pub struct ClockSynchronizer<T> {
    config: Arc<Config>,
    tuner_manager: T,
}

// TODO: The following implementation has code clones similar to
//       EitCollector and ServiceScanner.

impl<T> ClockSynchronizer<T>
where
    T: Clone,
    T: Call<StartStreaming>,
    T: TriggerFactory<StopStreaming>,
{
    const LABEL: &'static str = "epg.sync-clocks";

    pub fn new(config: Arc<Config>, tuner_manager: T) -> Self {
        ClockSynchronizer {
            config,
            tuner_manager,
        }
    }

    pub async fn sync_clocks(self) -> Vec<(EpgChannel, Option<HashMap<ServiceId, Clock>>)> {
        let command = &self.config.jobs.sync_clocks.command;
        let mut results = Vec::new();

        for channel in self.config.channels.iter() {
            let result =
                match Self::sync_clocks_in_channel(channel, command, &self.tuner_manager).await {
                    Ok(clocks) => {
                        let mut map = HashMap::new();
                        for clock in clocks.into_iter() {
                            let service_id = ServiceId::new(clock.nid, clock.sid);
                            map.insert(service_id, clock.clock.clone());
                        }
                        Some(map)
                    }
                    Err(err) => {
                        tracing::error!(%err, channel.name, "Failed to synchronize clocks");
                        None
                    }
                };
            results.push((channel.clone().into(), result));
        }

        results
    }

    async fn sync_clocks_in_channel(
        channel: &ChannelConfig,
        command: &str,
        tuner_manager: &T,
    ) -> anyhow::Result<Vec<SyncClock>> {
        tracing::debug!(channel.name, "Synchronizing clocks...");

        let user = TunerUser {
            info: TunerUserInfo::Job(Self::LABEL.to_string()),
            priority: (-1).into(),
        };

        let stream = tuner_manager
            .call(StartStreaming {
                channel: channel.clone().into(),
                user,
                stream_id: None,
            })
            .await??;

        let msg = StopStreaming { id: stream.id() };
        let stop_trigger = tuner_manager.trigger(msg);

        let template = mustache::compile_str(command)?;
        let data = mustache::MapBuilder::new()
            .insert("sids", &channel.services)?
            .insert("xsids", &channel.excluded_services)?
            .build();
        let cmd = template.render_data_to_string(&data)?;

        let mut pipeline = command_util::spawn_pipeline(vec![cmd], stream.id(), Self::LABEL)?;

        let (input, mut output) = pipeline.take_endpoints();

        let handle = tokio::spawn(stream.pipe(input).in_current_span());

        let mut buf = Vec::new();
        output.read_to_end(&mut buf).await?;

        drop(stop_trigger);

        // Explicitly dropping the output of the pipeline is needed.  The output
        // holds the child processes and it kills them when dropped.
        drop(pipeline);

        // Wait for the task so that the tuner is released before a request for
        // streaming in the next iteration.
        let _ = handle.await;

        anyhow::ensure!(!buf.is_empty(), "No clock, maybe out of service");

        let clocks: Vec<SyncClock> = serde_json::from_slice(&buf)?;
        tracing::debug!(
            channel.name,
            clocks.len = clocks.len(),
            "Synchronized clocks"
        );

        Ok(clocks)
    }
}

#[derive(Clone, Deserialize)]
#[cfg_attr(test, derive(Serialize))]
#[serde(rename_all = "camelCase")]
pub struct SyncClock {
    pub nid: Nid,
    #[allow(dead_code)]
    pub tsid: Tsid,
    pub sid: Sid,
    pub clock: Clock,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tuner::stub::TunerManagerStub;
    use assert_matches::assert_matches;
    use test_log::test;

    #[test(tokio::test)]
    async fn test_sync_clocks_in_channel() {
        let stub = TunerManagerStub::default();

        let expected = vec![SyncClock {
            nid: 1.into(),
            tsid: 2.into(),
            sid: 3.into(),
            clock: Clock {
                pid: 1,
                pcr: 2,
                time: 3,
            },
        }];

        let config_yml = format!(
            r#"
            channels:
              - name: channel
                type: GR
                channel: '0'
            jobs:
              sync-clocks:
                command: echo '{}'
        "#,
            serde_json::to_string(&expected).unwrap()
        );

        let config = Arc::new(serde_yml::from_str::<Config>(&config_yml).unwrap());

        let sync = ClockSynchronizer::new(config, stub.clone());
        let results = sync.sync_clocks().await;
        assert_eq!(results.len(), 1);
        assert_matches!(&results[0], (_, Some(v)) => {
            let service_id = (1, 3).into();
            assert_eq!(v.len(), 1);
            assert!(v.contains_key(&service_id));
            assert_matches!(v[&service_id], Clock { pid, pcr, time } => {
                assert_eq!(pid, 1);
                assert_eq!(pcr, 2);
                assert_eq!(time, 3);
            });
        });

        // Emulate out of services by using `false`
        let config = Arc::new(
            serde_yml::from_str::<Config>(
                r#"
            channels:
              - name: channel
                type: GR
                channel: '0'
            jobs:
              scan-services:
                command: false
        "#,
            )
            .unwrap(),
        );
        let sync = ClockSynchronizer::new(config, stub.clone());
        let results = sync.sync_clocks().await;
        assert_eq!(results.len(), 1);
        assert_matches!(&results[0], (_, None));
    }
}
