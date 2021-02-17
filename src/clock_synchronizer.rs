use std::collections::HashMap;

use actix::prelude::*;
use failure::Error;
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

pub struct ClockSynchronizer<A: Actor> {
    command: String,
    channels: Vec<EpgChannel>,
    tuner_manager: Addr<A>,
}

// TODO: The following implementation has code clones similar to
//       EitCollector and ServiceScanner.

impl<A> ClockSynchronizer<A>
where
    A: Actor,
    A: Handler<StartStreamingMessage>,
    A: Handler<StopStreamingMessage>,
    A::Context: actix::dev::ToEnvelope<A, StartStreamingMessage>,
    A::Context: actix::dev::ToEnvelope<A, StopStreamingMessage>,
{
    const LABEL: &'static str = "clock-synchronizer";

    pub fn new(
        command: String,
        channels: Vec<EpgChannel>,
        tuner_manager: Addr<A>,
    ) -> Self {
        ClockSynchronizer { command, channels, tuner_manager }
    }

    pub async fn sync_clocks(
        self
    ) -> Vec<(EpgChannel, Option<HashMap<ServiceTriple, Clock>>)> {
        log::debug!("Synchronizing clocks...");

        let mut results = Vec::new();

        for channel in self.channels.iter() {
            let result = match Self::sync_clocks_in_channel(
                &channel, &self.command, &self.tuner_manager).await {
                Ok(clocks) => {
                    let mut map = HashMap::new();
                    for clock in clocks.into_iter() {
                        let triple = (clock.nid, clock.tsid, clock.sid).into();
                        map.insert(triple, clock.clock.clone());
                    }
                    Some(map)
                }
                Err(err) => {
                    log::warn!("Failed to synchronize clocks in {}: {}",
                               channel.name, err);
                    None
                }
            };
            results.push((channel.clone(), result));
        }

        log::debug!("Synchronized {} channels", self.channels.len());

        results
    }

    async fn sync_clocks_in_channel(
        channel: &EpgChannel,
        command: &str,
        tuner_manager: &Addr<A>,
    ) -> Result<Vec<SyncClock>, Error> {
        log::debug!("Synchronizing clocks in {}...", channel.name);

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

        failure::ensure!(!buf.is_empty(), "No clock, maybe out of service");

        let clocks: Vec<SyncClock> = serde_json::from_slice(&buf)?;
        log::debug!("Synchronized {} clocks in {}", clocks.len(), channel.name);

        Ok(clocks)
    }
}

#[derive(Clone, Deserialize)]
#[cfg_attr(test, derive(Serialize))]
#[serde(rename_all = "camelCase")]
struct SyncClock {
    nid: NetworkId,
    tsid: TransportStreamId,
    sid: ServiceId,
    clock: Clock,
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::*;
    use crate::broadcaster::BroadcasterStream;
    use crate::error::Error;
    use crate::mpeg_ts_stream::MpegTsStream;

    type Mock = actix::actors::mocker::Mocker<TunerManager>;

    #[actix_rt::test]
    async fn test_sync_clocks_in_channel() {
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

        let expected = vec![SyncClock {
            nid: 1.into(),
            tsid: 2.into(),
            sid: 3.into(),
            clock: Clock { pid: 1, pcr: 2, time: 3 },
        }];

        let cmd = format!(
            "echo '{}'", serde_json::to_string(&expected).unwrap());
        let sync = ClockSynchronizer::new(cmd, channels.clone(), mock.clone());
        let results = sync.sync_clocks().await;
        assert_eq!(results.len(), 1);
        assert_matches!(&results[0], (_, Some(v)) => {
            let triple = (1, 2, 3).into();
            assert_eq!(v.len(), 1);
            assert!(v.contains_key(&triple));
            assert_matches!(v[&triple], Clock { pid, pcr, time } => {
                assert_eq!(pid, 1);
                assert_eq!(pcr, 2);
                assert_eq!(time, 3);
            });
        });

        // Emulate out of services by using `false`
        let cmd = "false".to_string();
        let sync = ClockSynchronizer::new(cmd, channels.clone(), mock.clone());
        let results = sync.sync_clocks().await;
        assert_eq!(results.len(), 1);
        assert_matches!(&results[0], (_, None));
    }
}
