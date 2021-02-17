use actix::prelude::*;
use failure::Error;
use indexmap::IndexMap;
use log;
use serde::Deserialize;
use serde_json;
use tokio::io::AsyncReadExt;

#[cfg(test)]
use serde::Serialize;

use crate::command_util;
use crate::epg::*;
use crate::models::*;
use crate::tuner::*;

pub struct ServiceScanner<A: Actor> {
    command: String,
    channels: Vec<EpgChannel>,
    tuner_manager: Addr<A>,
}

// TODO: The following implementation has code clones similar to
//       ClockSynchronizer and EitCollector.

impl<A> ServiceScanner<A>
where
    A: Actor,
    A: Handler<StartStreamingMessage>,
    A: Handler<StopStreamingMessage>,
    A::Context: actix::dev::ToEnvelope<A, StartStreamingMessage>,
    A::Context: actix::dev::ToEnvelope<A, StopStreamingMessage>,
{
    const LABEL: &'static str = "service-scanner";

    pub fn new(
        command: String,
        channels: Vec<EpgChannel>,
        tuner_manager: Addr<A>,
    ) -> Self {
        ServiceScanner { command, channels, tuner_manager }
    }

    pub async fn scan_services(
        self
    ) -> Vec<(EpgChannel, Option<IndexMap<ServiceTriple, EpgService>>)> {
        log::debug!("Scanning services...");

        let mut results = Vec::new();
        for channel in self.channels.iter() {
            let result = match Self::scan_services_in_channel(
                &channel, &self.command, &self.tuner_manager).await {
                Ok(services) => {
                    let mut map = IndexMap::new();
                    for service in services.into_iter() {
                        map.insert(service.triple(), service.clone());
                    }
                    Some(map)
                }
                Err(err) => {
                    log::warn!("Failed to scan services in {}: {}",
                               channel.name, err);
                    None
                }
            };
            results.push((channel.clone(), result));
        }

        log::debug!("Scanned {} channels", self.channels.len());

        results
    }

    async fn scan_services_in_channel(
        channel: &EpgChannel,
        command: &str,
        tuner_manager: &Addr<A>,
    ) -> Result<Vec<EpgService>, Error> {
        log::debug!("Scanning services in {}...", channel.name);

        let user = TunerUser {
            info: TunerUserInfo::Job { name: Self::LABEL.to_string() },
            priority: (-1).into(),
        };

        let stream = tuner_manager.send(StartStreamingMessage {
            channel: channel.clone(),
            user
        }).await??;

        let stop_trigger = TunerStreamStopTrigger::new(
            stream.id(), tuner_manager.clone().recipient());

        let template = mustache::compile_str(command)?;
        let data = mustache::MapBuilder::new()
            .insert("sids", &channel.services)?
            .insert("xsids", &channel.excluded_services)?
            .build();
        let cmd = template.render_data_to_string(&data)?;

        let mut pipeline = command_util::spawn_pipeline(
            vec![cmd], stream.id())?;

        let (input, mut output) = pipeline.take_endpoints().unwrap();

        let handle = tokio::spawn(stream.pipe(input));

        let mut buf = Vec::new();
        output.read_to_end(&mut buf).await?;

        drop(stop_trigger);

        // Explicitly dropping the output of the pipeline is needed.  The output
        // holds the child processes and it kills them when dropped.
        drop(pipeline);

        // Wait for the task so that the tuner is released before a request for
        // streaming in the next iteration.
        let _ = handle.await;

        failure::ensure!(buf.len() > 0, "No service, maybe out of service");

        let services: Vec<TsService> = serde_json::from_slice(&buf)?;
        log::debug!("Found {} services in {}", services.len(), channel.name);

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
    nid: NetworkId,
    tsid: TransportStreamId,
    sid: ServiceId,
    #[serde(rename = "type")]
    service_type: u16,
    #[serde(default)]
    logo_id: i16,
    #[serde(default)]
    remote_control_key_id: u16,
    name: String,
}

impl From<(&EpgChannel, &TsService)> for EpgService {
    fn from((ch, sv): (&EpgChannel, &TsService)) -> Self {
        EpgService {
            nid: sv.nid,
            tsid: sv.tsid,
            sid: sv.sid,
            service_type: sv.service_type,
            logo_id: sv.logo_id,
            remote_control_key_id: sv.remote_control_key_id,
            name: sv.name.clone(),
            channel: ch.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::broadcaster::BroadcasterStream;
    use crate::error::Error;
    use crate::mpeg_ts_stream::MpegTsStream;

    type Mock = actix::actors::mocker::Mocker<TunerManager>;

    #[actix_rt::test]
    async fn test_scan_services_in_channel() {
        let mock = Mock::mock(Box::new(|msg, _ctx| {
            if let Some(_) = msg.downcast_ref::<StartStreamingMessage>() {
                let (_, stream) = BroadcasterStream::new_for_test();
                let result: Result<_, Error> =
                    Ok(MpegTsStream::new(TunerSubscriptionId::default(), stream));
                Box::new(Some(result))
            } else if let Some(_) = msg.downcast_ref::<StopStreamingMessage>() {
                Box::new(Some(()))
            } else {
                unimplemented!();
            }
        })).start();

        let channels = vec![EpgChannel {
            name: "channel".to_string(),
            channel_type: ChannelType::GR,
            channel: "0".to_string(),
            extra_args: "".to_string(),
            services: vec![],
            excluded_services: vec![],
        }];

        let expected = vec![TsService {
            nid: 1.into(),
            tsid: 2.into(),
            sid: 3.into(),
            service_type: 4,
            logo_id: 0,
            remote_control_key_id: 1,
            name: "service".to_string(),
        }];

        let cmd = format!(
            "echo '{}'", serde_json::to_string(&expected).unwrap());
        let scan = ServiceScanner::new(cmd, channels.clone(), mock.clone());
        let results = scan.scan_services().await;
        assert!(results[0].1.is_some());
        assert_eq!(results[0].1.as_ref().unwrap().len(), 1);

        // Emulate out of services by using `false`
        let cmd = "false".to_string();
        let scan = ServiceScanner::new(cmd, channels.clone(), mock.clone());
        let results = scan.scan_services().await;
        assert!(results[0].1.is_none());
    }
}
