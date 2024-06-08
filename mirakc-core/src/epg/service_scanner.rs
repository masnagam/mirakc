use std::sync::Arc;

use actlet::prelude::*;
use indexmap::IndexMap;
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

pub struct ServiceScanner<T> {
    config: Arc<Config>,
    tuner_manager: T,
}

// TODO: The following implementation has code clones similar to
//       ClockSynchronizer and EitCollector.

impl<T> ServiceScanner<T>
where
    T: Clone,
    T: Call<StartStreaming>,
    T: TriggerFactory<StopStreaming>,
{
    const LABEL: &'static str = "epg.scan-services";

    pub fn new(config: Arc<Config>, tuner_manager: T) -> Self {
        ServiceScanner {
            config,
            tuner_manager,
        }
    }

    pub async fn scan_services(self) -> Vec<(EpgChannel, Option<IndexMap<ServiceId, EpgService>>)> {
        let command = &self.config.jobs.scan_services.command;
        let mut results = Vec::new();

        for channel in self.config.channels.iter() {
            let result =
                match Self::scan_services_in_channel(channel, command, &self.tuner_manager).await {
                    Ok(services) => {
                        let mut map = IndexMap::new();
                        for service in services.into_iter() {
                            map.insert(service.id, service.clone());
                        }
                        Some(map)
                    }
                    Err(err) => {
                        tracing::error!(%err, channel.name, "Failed to scan services");
                        None
                    }
                };
            results.push((channel.clone().into(), result));
        }

        results
    }

    async fn scan_services_in_channel(
        channel: &ChannelConfig,
        command: &str,
        tuner_manager: &T,
    ) -> anyhow::Result<Vec<EpgService>> {
        tracing::debug!(channel.name, "Scanning services...");

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

        anyhow::ensure!(!buf.is_empty(), "No service, maybe out of service");

        let services: Vec<TsService> = serde_json::from_slice(&buf)?;
        tracing::debug!(
            channel.name,
            services.len = services.len(),
            "Found services"
        );

        Ok(services
            .into_iter()
            .map(|sv| EpgService::from((channel, &sv)))
            .collect())
    }
}

#[derive(Clone, Deserialize)]
#[cfg_attr(test, derive(Serialize))]
#[serde(rename_all = "camelCase")]
struct TsService {
    nid: Nid,
    #[allow(dead_code)]
    tsid: Tsid,
    sid: Sid,
    #[serde(rename = "type")]
    service_type: u16,
    #[serde(default)]
    logo_id: i16,
    #[serde(default)]
    remote_control_key_id: u16,
    name: String,
}

impl From<(&ChannelConfig, &TsService)> for EpgService {
    fn from((ch, sv): (&ChannelConfig, &TsService)) -> Self {
        EpgService {
            id: ServiceId::new(sv.nid, sv.sid),
            service_type: sv.service_type,
            logo_id: sv.logo_id,
            remote_control_key_id: sv.remote_control_key_id,
            name: sv.name.clone(),
            channel: ch.clone().into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tuner::stub::TunerManagerStub;
    use test_log::test;

    #[test(tokio::test)]
    async fn test_scan_services_in_channel() {
        let stub = TunerManagerStub::default();

        let expected = vec![TsService {
            nid: 1.into(),
            tsid: 2.into(),
            sid: 3.into(),
            service_type: 4,
            logo_id: 0,
            remote_control_key_id: 1,
            name: "service".to_string(),
        }];

        let config_yml = format!(
            r#"
            channels:
              - name: channel
                type: GR
                channel: '0'
            jobs:
              scan-services:
                command: echo '{}'
        "#,
            serde_json::to_string(&expected).unwrap()
        );

        let config = Arc::new(serde_yml::from_str::<Config>(&config_yml).unwrap());

        let scan = ServiceScanner::new(config, stub.clone());
        let results = scan.scan_services().await;
        assert!(results[0].1.is_some());
        assert_eq!(results[0].1.as_ref().unwrap().len(), 1);

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
        let scan = ServiceScanner::new(config, stub.clone());
        let results = scan.scan_services().await;
        assert!(results[0].1.is_none());
    }
}
