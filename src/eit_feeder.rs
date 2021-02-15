use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;

use actix::prelude::*;
use chrono::{DateTime, Duration};
use log;
use serde::{Deserialize, Serialize};
use serde_json;
use tokio::prelude::*;
use tokio::io::BufReader;

use crate::config::Config;
use crate::datetime_ext::*;
use crate::error::Error;
use crate::epg::*;
use crate::models::*;
use crate::tuner::*;
use crate::command_util;

pub fn start(
    config: Arc<Config>,
    tuner_manager: Addr<TunerManager>,
    epg: Addr<Epg>,
) -> Addr<EitFeeder> {
    EitFeeder::new(config, tuner_manager, epg).start()
}

pub struct EitFeeder {
    config: Arc<Config>,
    tuner_manager: Addr<TunerManager>,
    epg: Addr<Epg>,
}

impl EitFeeder {
    pub fn new(
        config: Arc<Config>,
        tuner_manager: Addr<TunerManager>,
        epg: Addr<Epg>,
    ) -> Self {
        EitFeeder { config, tuner_manager, epg }
    }

    async fn feed_eit_sections(
        command: String,
        tuner_manager: Addr<TunerManager>,
        epg: Addr<Epg>,
    ) -> Result<(), Error> {
        let services = epg.send(QueryServicesMessage).await??;

        let mut map: HashMap<NetworkId, EpgChannel> = HashMap::new();
        for sv in services.iter() {
            map.entry(sv.nid)
                .and_modify(|ch| ch.services.push(sv.sid))
                .or_insert(EpgChannel {
                    name: sv.channel.name.clone(),
                    channel_type: sv.channel.channel_type,
                    channel: sv.channel.channel.clone(),
                    extra_args: sv.channel.extra_args.clone(),
                    services: vec![sv.sid],
                    excluded_services: vec![],
                });
        }
        let channels = map.values().cloned().collect();

        EitCollector::new(command, channels, tuner_manager, epg)
            .collect_schedules().await
    }
}

impl Actor for EitFeeder {
    type Context = Context<Self>;

    fn started(&mut self, _: &mut Self::Context) {
        log::debug!("Started");
    }

    fn stopped(&mut self, _: &mut Self::Context) {
        log::debug!("Stopped");
    }
}

// feed eit sections

pub struct FeedEitSectionsMessage;

impl fmt::Display for FeedEitSectionsMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FeedEitSections")
    }
}

impl Message for FeedEitSectionsMessage {
    type Result = Result<(), Error>;
}

impl Handler<FeedEitSectionsMessage> for EitFeeder {
    type Result = Response<(), Error>;

    fn handle(
        &mut self,
        msg: FeedEitSectionsMessage,
        _: &mut Self::Context,
    ) -> Self::Result {
        log::debug!("{}", msg);
        let fut = Box::pin(Self::feed_eit_sections(
            self.config.jobs.update_schedules.command.clone(),
            self.tuner_manager.clone(), self.epg.clone()));
        Response::fut(fut)
    }
}

// collector

pub struct EitCollector {
    command: String,
    channels: Vec<EpgChannel>,
    tuner_manager: Addr<TunerManager>,
    epg: Addr<Epg>,
}

// TODO: The following implementation has code clones similar to
//       ClockSynchronizer and ServiceScanner.

impl EitCollector {
    const LABEL: &'static str = "eit-collector";
    const UPDATE_CHUNK_SIZE: usize = 32;

    pub fn new(
        command: String,
        channels: Vec<EpgChannel>,
        tuner_manager: Addr<TunerManager>,
        epg: Addr<Epg>,
    ) -> Self {
        EitCollector { command, channels, tuner_manager, epg }
    }

    pub async fn collect_schedules(
        self
    ) -> Result<(), Error> {
        log::info!("Collecting EIT sections...");
        let mut num_sections = 0;
        for channel in self.channels.iter() {
            num_sections += Self::collect_eits_in_channel(
                &channel, &self.command, &self.tuner_manager, &self.epg).await?;
        }
        log::info!("Collected {} EIT sections", num_sections);
        Ok(())
    }

    async fn collect_eits_in_channel(
        channel: &EpgChannel,
        command: &str,
        tuner_manager: &Addr<TunerManager>,
        epg: &Addr<Epg>,
    ) -> Result<usize, Error> {
        log::debug!("Collecting EIT sections in {}...", channel.name);

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

        let (input, output) = pipeline.take_endpoints().unwrap();

        let handle = tokio::spawn(stream.pipe(input));

        let mut reader = BufReader::new(output);
        let mut json = String::new();
        let mut num_sections = 0;
        let mut triples = HashSet::new();
        let mut sections = Vec::with_capacity(Self::UPDATE_CHUNK_SIZE);
        while reader.read_line(&mut json).await? > 0 {
            let eit = serde_json::from_str::<EitSection>(&json)?;
            triples.insert(eit.service_triple());
            sections.push(eit);
            if sections.len() == Self::UPDATE_CHUNK_SIZE {
                epg.do_send(UpdateSchedulesMessage { sections });
                sections = Vec::with_capacity(32);
            }
            json.clear();
            num_sections += 1;
        }
        if !sections.is_empty() {
            epg.do_send(UpdateSchedulesMessage { sections });
        }

        drop(stop_trigger);

        // Explicitly dropping the output of the pipeline is needed.  The output
        // holds the child processes and it kills them when dropped.
        drop(pipeline);

        // Wait for the task so that the tuner is released before a request for
        // streaming in the next iteration.
        let _ = handle.await;

        epg.do_send(FlushSchedulesMessage {
            triples: triples.into_iter().collect(),
        });

        log::debug!("Collected {} EIT sections in {}",
                    num_sections, channel.name);

        Ok(num_sections)
    }
}

#[derive(Clone)]
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EitSection {
    pub original_network_id: NetworkId,
    pub transport_stream_id: TransportStreamId,
    pub service_id: ServiceId,
    pub table_id: u16,
    pub section_number: u8,
    pub last_section_number: u8,
    pub segment_last_section_number: u8,
    pub version_number: u8,
    pub events: Vec<EitEvent>,
}

impl EitSection {
    pub fn table_index(&self) -> usize {
        self.table_id as usize - 0x50
    }

    pub fn segment_index(&self) -> usize {
        self.section_number as usize / 8
    }

    pub fn section_index(&self) -> usize {
        self.section_number as usize % 8
    }

    pub fn last_section_index(&self) -> usize {
        self.segment_last_section_number as usize % 8
    }

    pub fn service_triple(&self) -> ServiceTriple {
        (self.original_network_id,
         self.transport_stream_id,
         self.service_id).into()
    }
}

#[derive(Clone)]
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EitEvent {
    pub event_id: EventId,
    #[serde(with = "serde_jst")]
    pub start_time: DateTime<Jst>,
    #[serde(with = "serde_duration_in_millis")]
    pub duration: Duration,
    pub scrambled: bool,
    pub descriptors: Vec<EitDescriptor>,
}

impl EitEvent {
    pub fn end_time(&self) -> DateTime<Jst> {
        self.start_time + self.duration
    }

    pub fn is_overnight_event(&self, midnight: DateTime<Jst>) -> bool {
        self.start_time < midnight && self.end_time() > midnight
    }
}

#[derive(Clone)]
#[derive(Deserialize, Serialize)]
#[serde(tag = "$type")]
pub enum EitDescriptor {
    #[serde(rename_all = "camelCase")]
    ShortEvent {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        event_name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
    },
    Component(ComponentDescriptor),
    AudioComponent(AudioComponentDescriptor),
    #[serde(rename_all = "camelCase")]
    Content {
        nibbles: Vec<(u8, u8, u8, u8)>,
    },
    #[serde(rename_all = "camelCase")]
    ExtendedEvent {
        items: Vec<(String, String)>,
    },
}

#[derive(Clone)]
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ComponentDescriptor {
    pub stream_content: u8,
    pub component_type: u8,
    pub component_tag: u8,
    pub language_code: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

#[derive(Clone)]
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioComponentDescriptor {
    pub stream_content: u8,
    pub component_type: u8,
    pub component_tag: u8,
    pub simulcast_group_tag: u8,
    pub es_multi_lingual_flag: bool,
    pub main_component_flag: bool,
    pub quality_indicator: u8,
    pub sampling_rate: u8,
    pub language_code: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language_code2: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}
