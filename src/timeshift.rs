use std::collections::HashMap;
use std::fmt;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use actix::prelude::*;
use chrono::DateTime;
use indexmap::IndexMap;
use serde::Deserialize;
use tokio::prelude::*;
use tokio::io::{AsyncSeek, BufReader, SeekFrom, Take};
use tokio::fs::File;
use tokio::sync::oneshot;

use crate::config::{Config, TimeshiftConfig};
use crate::chunk_stream::*;
use crate::datetime_ext::*;
use crate::eit_feeder::*;
use crate::error::Error;
use crate::epg::*;
use crate::filter::*;
use crate::models::*;
use crate::mpeg_ts_stream::*;
use crate::tuner::*;
use crate::command_util::{
    spawn_pipeline, CommandPipeline, CommandPipelineOutput, CommandPipelineProcessModel,
};

pub fn start(
    config: Arc<Config>,
    tuner_manager: Addr<TunerManager>,
) -> Addr<TimeshiftManager> {
    TimeshiftManager::new(config, tuner_manager).start()
}

// timeshift manager

type TimeshiftStream = MpegTsStream<String, ChunkStream<TimeshiftFileReader>>;
type TimeshiftRecordStream = MpegTsStream<String, ChunkStream<Take<TimeshiftFileReader>>>;

pub struct TimeshiftManager {
    config: Arc<Config>,
    tuner_manager: Addr<TunerManager>,
    recorders: HashMap<String, Addr<TimeshiftRecorder>>,
}

impl TimeshiftManager {
    pub fn new(config: Arc<Config>, tuner_manager: Addr<TunerManager>) -> Self {
        let recorders = HashMap::new();
        TimeshiftManager { config, tuner_manager, recorders, }
    }
}

impl Actor for TimeshiftManager {
    type Context = actix::Context<Self>;

    fn started(&mut self, _: &mut Self::Context) {
        log::debug!("Started");
        let mut recorders = HashMap::new();
        for name in self.config.timeshift.keys() {
            let recorder = TimeshiftRecorder::new(name.clone(), self.config.clone());
            recorders.insert(name.clone(), recorder.start());
        }
        self.recorders = recorders;
    }

    fn stopped(&mut self, _: &mut Self::Context) {
        log::debug!("Stopped");
    }
}

#[derive(Message)]
#[rtype(result = "Result<Vec<TimeshiftRecorderModel>, Error>")]
pub struct QueryTimeshiftRecordersMessage;

impl fmt::Display for QueryTimeshiftRecordersMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "QueryTimeshiftRecorders")
    }
}

impl Handler<QueryTimeshiftRecordersMessage> for TimeshiftManager {
    type Result = ResponseFuture<Result<Vec<TimeshiftRecorderModel>, Error>>;

    fn handle(
        &mut self,
        msg: QueryTimeshiftRecordersMessage,
        _: &mut Self::Context,
    ) -> Self::Result {
        log::debug!("{}", msg);
        let futures: Vec<Request<TimeshiftRecorder, QueryTimeshiftRecorderMessage>> =
            self.recorders.iter()
            .map(|(name, recorder)| {
                recorder.send(QueryTimeshiftRecorderMessage {
                    recorder_name: name.clone(),
                })
            })
            .collect();
        Box::pin(async move {
            let recorders: Result<Vec<TimeshiftRecorderModel>, Error> =
                futures::future::join_all(futures)
                .await
                .into_iter()
                .flatten()
                .collect();
            recorders
        })
    }
}

macro_rules! impl_proxy_handler {
    ($msg:ident, $result:path) => {
        impl Handler<$msg> for TimeshiftManager {
            type Result = ResponseFuture<$result>;

            fn handle(
                &mut self,
                msg: $msg,
                _ctx: &mut Self::Context,
            ) -> Self::Result {
                log::debug!("{}", msg);
                let maybe_recorder = self.recorders
                    .get(&msg.recorder_name)
                    .cloned()
                    .ok_or(Error::RecordNotFound);
                Box::pin(async move {
                    maybe_recorder?.send(msg).await?
                })
            }
        }
    }
}

#[derive(Message)]
#[rtype(result = "Result<TimeshiftRecorderModel, Error>")]
pub struct QueryTimeshiftRecorderMessage {
    pub recorder_name: String,
}

impl fmt::Display for QueryTimeshiftRecorderMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "QueryTimeshiftRecorder for {}", self.recorder_name)
    }
}

impl_proxy_handler!(QueryTimeshiftRecorderMessage, Result<TimeshiftRecorderModel, Error>);

#[derive(Message)]
#[rtype(result = "Result<Vec<TimeshiftRecordModel>, Error>")]
pub struct QueryTimeshiftRecordsMessage {
    pub recorder_name: String,
}

impl fmt::Display for QueryTimeshiftRecordsMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "QueryTimeshiftRecords in {}", self.recorder_name)
    }
}

impl_proxy_handler!(QueryTimeshiftRecordsMessage, Result<Vec<TimeshiftRecordModel>, Error>);

#[derive(Message)]
#[rtype(result = "Result<TimeshiftRecordModel, Error>")]
pub struct QueryTimeshiftRecordMessage {
    pub recorder_name: String,
    pub record_id: TimeshiftRecordId,
}

impl fmt::Display for QueryTimeshiftRecordMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "QueryTimeshiftRecord for Record#{} in {}",
               self.record_id, self.recorder_name)
    }
}

impl_proxy_handler!(QueryTimeshiftRecordMessage, Result<TimeshiftRecordModel, Error>);

#[derive(Message)]
#[rtype(result = "Result<(TimeshiftStream, TimeshiftStreamStopTrigger), Error>")]
pub struct StartTimeshiftStreamingMessage {
    pub recorder_name: String,
    pub record_id: Option<TimeshiftRecordId>,
}

impl fmt::Display for StartTimeshiftStreamingMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(id) = self.record_id {
            write!(f, "StartTimeshiftStreaming for Record#{} in {}",
                   id, self.recorder_name)
        } else {
            write!(f, "StartTimeshiftStreaming for {}", self.recorder_name)
        }
    }
}

impl_proxy_handler!(
    StartTimeshiftStreamingMessage,
    Result<(TimeshiftStream, TimeshiftStreamStopTrigger), Error>);

#[derive(Message)]
#[rtype(result = "Result<(TimeshiftRecordStream, TimeshiftStreamStopTrigger), Error>")]
pub struct StartTimeshiftRecordStreamingMessage {
    pub recorder_name: String,
    pub record_id: TimeshiftRecordId,
    pub start_pos: Option<u64>,
}

impl fmt::Display for StartTimeshiftRecordStreamingMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(pos) = self.start_pos {
            write!(f, "StartTimeshiftRecordStreaming from {} of Record#{} in {}",
                   pos, self.record_id, self.recorder_name)
        } else {
            write!(f, "StartTimeshiftRecordStreaming from the beginning of Record#{} in {}",
                   self.record_id, self.recorder_name)
        }
    }
}

impl_proxy_handler!(
    StartTimeshiftRecordStreamingMessage,
    Result<(TimeshiftRecordStream, TimeshiftStreamStopTrigger), Error>);

impl Handler<NotifyServicesUpdatedMessage> for TimeshiftManager {
    type Result = ();

    fn handle(
        &mut self,
        msg: NotifyServicesUpdatedMessage,
        _ctx: &mut Self::Context,
    ) -> Self::Result {
        log::debug!("{}", msg);
        for (name, config) in self.config.clone().timeshift.iter() {
            let triple = ServiceTriple::from(config.service_triple.clone());
            if msg.services.contains_key(&triple) {
                log::info!("{}: Service#{} is available, activate", name, triple);
                let service = msg.services[&triple].clone();
                self.recorders[name].do_send(ActivateTimeshiftRecorderMessage {
                    service,
                    tuner_manager: self.tuner_manager.clone(),
                });
            } else {
                log::warn!("{}: Service#{} is unavailable, deactivate", name, triple);
                self.recorders[name].do_send(DeactivateTimeshiftRecorderMessage);
            }
        }
    }
}

// recorder

struct TimeshiftRecorder {
    name: String,
    config: Arc<Config>,
    service: EpgService,
    records: IndexMap<TimeshiftRecordId, TimeshiftRecord>,
    points: Vec<TimeshiftPoint>,
    session: Option<TimeshiftRecorderSession>,
}

impl TimeshiftRecorder {
    fn new(name: String, config: Arc<Config>) -> Self {
        let timeshift_config = &config.timeshift[&name];
        let service = EpgService {  // dummy data
            nid: timeshift_config.service_triple.0,
            tsid: timeshift_config.service_triple.1,
            sid: timeshift_config.service_triple.2,
            service_type: 0,
            logo_id: 0,
            remote_control_key_id: 0,
            name: "".to_string(),
            channel: EpgChannel {
                name: "".to_string(),
                channel_type: ChannelType::GR,
                channel: "".to_string(),
                extra_args: "".to_string(),
                services: vec![],
                excluded_services: vec![],
            },
        };
        let max_chunks = timeshift_config.max_chunks();
        TimeshiftRecorder {
            name,
            config,
            service,
            records: IndexMap::new(),
            points: Vec::with_capacity(max_chunks),
            session: None,
        }
    }

    fn config(&self) -> &TimeshiftConfig {
        &self.config.timeshift[&self.name]
    }

    fn deactivate(&mut self) {
        if self.session.is_some() {
            log::warn!("{}: Deactivated, but inactive", self.name);
        } else {
            log::info!("{}: Deactivated", self.name);
        }
        self.session = None;
    }

    fn create_live_stream_source(
        &self,
        record_id: Option<TimeshiftRecordId>,
    ) -> Result<TimeshiftStreamSource, Error> {
        if self.points.len() < 2 {
            return Err(Error::RecordNotFound)
        }
        let name = self.name.clone();
        let file = self.config().file.clone();
        let point = if let Some(id) = record_id {
            let record = self.records.get(&id).ok_or(Error::ProgramNotFound)?;
            record.start.clone()
        } else {
            self.points[0].clone()
        };
        Ok(TimeshiftStreamSource { name, file, point })
    }

    fn create_record_stream_source(
        &self,
        record_id: TimeshiftRecordId,
        start_pos: Option<u64>,
    ) -> Result<TimeshiftRecordStreamSource, Error> {
        let record = self.records.get(&record_id).ok_or(Error::ProgramNotFound)?;
        record.create_record_stream_source(self.name.clone(), self.config(), start_pos)
    }

    fn handle_start_recording(&mut self) {
        log::info!("{}: Started recording", self.name);
    }

    fn handle_stop_recording(&mut self, reset: bool) {
        log::info!("{}: Stopped recording", self.name);
        if reset {
            log::warn!("{}: Reset data", self.name);
        }
    }

    fn handle_chunk(&mut self, point: TimeshiftPoint) {
        self.maintain();
        self.append_point(point);
    }

    fn maintain(&mut self) {
        if self.points.len() < self.config().max_chunks() {
            return;
        }
        self.invalidate_first_chunk();
        self.purge_expired_records();
    }

    fn invalidate_first_chunk(&mut self) {
        assert!(self.points.len() == self.config().max_chunks());
        let point = self.points.remove(0);
        let index = point.pos / (self.config().chunk_size as u64);
        log::debug!("{}: Chunk#{}: Invalidated", self.name, index);
    }

    fn purge_expired_records(&mut self) {
        assert!(!self.points.is_empty());
        let timestamp = self.points[0].timestamp;  // timestamp of the first chunk
        let n = self.records.values()
            .position(|record| record.end.timestamp > timestamp)
            .unwrap_or(self.records.len());
        for (_, record) in self.records.drain(0..n) {  // remove first n records
            log::info!("{}: Record#{}: Purged: {}",
                       self.name, record.id, record.program.name());
        }
    }

    fn append_point(&mut self, point: TimeshiftPoint) {
        let index = point.pos / (self.config().chunk_size as u64);
        assert!(point.pos % (self.config().chunk_size as u64) == 0);
        log::debug!("{}: Chunk#{}: Timestamp: {}", self.name, index, point.timestamp);
        self.points.push(point);
        assert!(self.points.len() <= self.config().max_chunks());
    }

    fn handle_event_start(
        &mut self,
        quad: EventQuad,
        event: EitEvent,
        point: TimeshiftPoint,
    ) {
        let id = TimeshiftRecordId::from(point.timestamp.timestamp());
        let mut program = EpgProgram::new(quad);
        program.update(&event);
        log::info!("{}: Record#{}: Started: {}: {}", self.name, id, point, program.name());
        self.records.insert(id, TimeshiftRecord::new(id, program, point));
    }

    fn handle_event_update(
        &mut self,
        quad: EventQuad,
        event: EitEvent,
        point: TimeshiftPoint,
    ) {
        let mut program = EpgProgram::new(quad);
        program.update(&event);
        self.update_last_record(program, point, false);
    }

    fn handle_event_end(
        &mut self,
        quad: EventQuad,
        event: EitEvent,
        point: TimeshiftPoint,
    ) {
        let mut program = EpgProgram::new(quad);
        program.update(&event);
        self.update_last_record(program, point, true);
    }

    fn update_last_record(
        &mut self,
        program: EpgProgram,
        point: TimeshiftPoint,
        end: bool,
    ) {
        match self.records.values_mut().last() {
            Some(record) => {
                record.update(program, point, end);
                if end {
                    log::debug!("{}: Record#{}: Ended: {}: {}",
                                self.name, record.id, record.end, record.program.name());
                } else {
                    log::debug!("{}: Record#{}: Updated: {}: {}",
                                self.name, record.id, record.end, record.program.name());
                }
            }
            None => {
                log::warn!("{}: No record to update", self.name);
            }
        }
    }

    fn get_model(&self) -> TimeshiftRecorderModel {
        let now = Jst::now();
        let start_time = if let Some(point) = self.points.iter().next() {
            point.timestamp.clone()
        } else {
            now.clone()
        };
        let end_time = if let Some(point) = self.points.iter().last() {
            point.timestamp.clone()
        } else {
            now.clone()
        };
        let (pipeline, recording) = match self.session {
            Some(ref session) => (session.pipeline.get_model(), true),
            _ => (vec![], false),
        };
        TimeshiftRecorderModel {
            name: self.name.clone(),
            service: self.service.clone(),
            start_time,
            end_time,
            pipeline,
            recording,
        }
    }

    async fn activate(
        activation: TimeshiftActivation,
    ) -> Result<TimeshiftActivationResult, Error> {
        let config = activation.config.timeshift.get(&activation.name).unwrap();
        let channel = &activation.service.channel;

        let user = TunerUser {
            info: TunerUserInfo::Recorder {
                name: format!("timeshift({})", activation.name),
            },
            priority: config.priority.into(),
        };

        let stream = activation.tuner_manager.send(StartStreamingMessage {
            channel: channel.clone(),
            user,
        }).await??;

        // stop_trigger must be created here in order to stop streaming when an error occurs.
        let stop_trigger = TunerStreamStopTrigger::new(
            stream.id(), activation.tuner_manager.clone().recipient());

        let data = mustache::MapBuilder::new()
            .insert_str("channel_name", &channel.name)
            .insert("channel_type", &channel.channel_type)?
            .insert_str("channel", &channel.channel)
            .insert("sid", &activation.service.sid)?
            .build();
        let mut builder = FilterPipelineBuilder::new(data);
        // NOTE
        // ----
        // We always decode stream before recording in order to make it easy to support seeking.
        // It's impossible to decode stream started from any position in the record.  Only streams
        // starting with PAT packets can be decoded.  This means that we need to seek a PAT
        // packet before streaming and we cannot start streaming from a specific position that
        // is specified by the media player using a HTTP Range header.
        if !stream.is_decoded() {
            builder.add_decode_filter(&activation.config.filters.decode_filter)?;
        }
        let (mut cmds, _) = builder.build();

        let data = mustache::MapBuilder::new()
            .insert("sid", &activation.service.sid)?
            .insert_str("file", &config.file)
            .insert("chunk_size", &config.chunk_size)?
            .insert("num_chunks", &config.num_chunks)?
            .build();
        let template = mustache::compile_str(
            &activation.config.recorder.record_service_command)?;
        cmds.push(template.render_data_to_string(&data)?);

        let mut pipeline = spawn_pipeline(cmds, stream.id())?;

        let (input, output) = pipeline.take_endpoints()?;

        actix::spawn(async move {
            let _ = stream.pipe(input).await;
        });

        let session = TimeshiftRecorderSession {
            pipeline,
            _stop_trigger: stop_trigger,
        };

        Ok(TimeshiftActivationResult { session, output })
    }
}

impl Actor for TimeshiftRecorder {
    type Context = actix::Context<Self>;

    fn started(&mut self, _ctx: &mut Self::Context) {
        log::debug!("{}: Started", self.name);
    }

    fn stopped(&mut self, _ctx: &mut Self::Context) {
        log::debug!("{}: Stopped", self.name);
    }
}

#[derive(Message)]
#[rtype(result = "()")]
struct NotifyServiceUpdatedMessage {
    service: EpgService,
}

impl Handler<NotifyServiceUpdatedMessage> for TimeshiftRecorder {
    type Result = ();

    fn handle(
        &mut self,
        msg: NotifyServiceUpdatedMessage,
        _ctx: &mut Self::Context,
    ) -> Self::Result {
        self.service = msg.service;
    }
}

#[derive(Message)]
#[rtype(result = "()")]
struct ActivateTimeshiftRecorderMessage {
    service: EpgService,
    tuner_manager: Addr<TunerManager>,
}

impl Handler<ActivateTimeshiftRecorderMessage> for TimeshiftRecorder {
    type Result = ();

    fn handle(
        &mut self,
        msg: ActivateTimeshiftRecorderMessage,
        ctx: &mut Self::Context,
    ) -> Self::Result {
        self.service = msg.service;
        if self.session.is_none() {
            log::info!("{}: Start activation", self.name);
            let activation = TimeshiftActivation {
                config: self.config.clone(),
                name: self.name.clone(),
                service: self.service.clone(),
                tuner_manager: msg.tuner_manager.clone(),
            };
            Self::activate(activation)
                .into_actor(self)
                .map(|result, this, ctx| {
                    match result {
                        Ok(result) => {
                            log::debug!("{}: Activation finished successfully", this.name);
                            this.session = Some(result.session);
                            let reader = BufReader::new(result.output);
                            Self::add_stream(reader.lines(), ctx);
                        }
                        Err(err) => {
                            log::error!("{}: Activation failed: {}", this.name, err);
                        }
                    }
                })
                .wait(ctx);  // blocks until the activation finished
        } else {
            log::info!("{}: Already activated", self.name);
        }
    }
}

#[derive(Message)]
#[rtype(result = "()")]
struct DeactivateTimeshiftRecorderMessage;

impl Handler<DeactivateTimeshiftRecorderMessage> for TimeshiftRecorder {
    type Result = ();

    fn handle(
        &mut self,
        _msg: DeactivateTimeshiftRecorderMessage,
        _ctx: &mut Self::Context,
    ) -> Self::Result {
        self.deactivate();
    }
}

impl Handler<QueryTimeshiftRecorderMessage> for TimeshiftRecorder {
    type Result = MessageResult<QueryTimeshiftRecorderMessage>;

    fn handle(
        &mut self,
        _msg: QueryTimeshiftRecorderMessage,
        _ctx: &mut Self::Context,
    ) -> Self::Result {
        MessageResult(Ok(self.get_model()))
    }
}

impl Handler<QueryTimeshiftRecordsMessage> for TimeshiftRecorder {
    type Result = MessageResult<QueryTimeshiftRecordsMessage>;

    fn handle(
        &mut self,
        _msg: QueryTimeshiftRecordsMessage,
        _ctx: &mut Self::Context,
    ) -> Self::Result {
        let records: Vec<TimeshiftRecordModel> = self.records
            .values()
            .map(|record| record.get_model(self.config()))
            .collect();
        MessageResult(Ok(records))
    }
}

impl Handler<QueryTimeshiftRecordMessage> for TimeshiftRecorder {
    type Result = MessageResult<QueryTimeshiftRecordMessage>;

    fn handle(
        &mut self,
        msg: QueryTimeshiftRecordMessage,
        _: &mut Self::Context,
    ) -> Self::Result {
        let result = self.records
            .get(&msg.record_id)
            .map(|record| record.get_model(self.config()))
            .ok_or(Error::RecordNotFound);
        MessageResult(result)
    }
}

impl Handler<StartTimeshiftStreamingMessage> for TimeshiftRecorder {
    type Result = ResponseFuture<Result<(TimeshiftStream, TimeshiftStreamStopTrigger), Error>>;

    fn handle(
        &mut self,
        msg: StartTimeshiftStreamingMessage,
        _ctx: &mut Self::Context,
    ) -> Self::Result {
        let src = self.create_live_stream_source(msg.record_id);
        Box::pin(async move {
            src?.create_stream().await
        })
    }
}

impl Handler<StartTimeshiftRecordStreamingMessage> for TimeshiftRecorder {
    type Result =
        ResponseFuture<Result<(TimeshiftRecordStream, TimeshiftStreamStopTrigger), Error>>;

    fn handle(
        &mut self,
        msg: StartTimeshiftRecordStreamingMessage,
        _: &mut Self::Context,
    ) -> Self::Result {
        let src = self.create_record_stream_source(msg.record_id, msg.start_pos);
        Box::pin(async move {
            src?.create_stream().await
        })
    }
}

#[derive(Message)]
#[rtype(result = "()")]
pub struct NotifyTimeshiftRecordingPipelineBrokenMessage;

impl Handler<NotifyTimeshiftRecordingPipelineBrokenMessage> for TimeshiftRecorder {
    type Result = ();

    fn handle(
        &mut self,
        _msg: NotifyTimeshiftRecordingPipelineBrokenMessage,
        _ctx: &mut Self::Context,
    ) -> Self::Result {
        self.deactivate();
        // TODO: re-activate the recoder
    }
}

impl StreamHandler<io::Result<String>> for TimeshiftRecorder {
    fn handle(&mut self, line: io::Result<String>, _ctx: &mut actix::Context<Self>) {
        let json = match line {
            Ok(json) => json,
            Err(err) => {
                log::error!("{}: Failed reading output from the command pipeline: {}",
                            self.name, err);
                return;
            }
        };

        let msg = match serde_json::from_str::<TimeshiftRecorderMessage>(&json) {
            Ok(msg) => msg,
            Err(err) => {
                log::error!("{}: Failed parsing a JSON message: {}", self.name, err);
                return;
            }
        };

        match msg {
            TimeshiftRecorderMessage::Start => {
                self.handle_start_recording();
            }
            TimeshiftRecorderMessage::Stop(msg) => {
                self.handle_stop_recording(msg.reset);
            }
            TimeshiftRecorderMessage::Chunk(msg) => {
                self.handle_chunk(msg.chunk);
            }
            TimeshiftRecorderMessage::EventStart(msg) => {
                let quad = EventQuad::new(
                    msg.original_network_id,
                    msg.transport_stream_id,
                    msg.service_id,
                    msg.event.event_id);
                self.handle_event_start(quad, msg.event, msg.record);
            }
            TimeshiftRecorderMessage::EventUpdate(msg) => {
                let quad = EventQuad::new(
                    msg.original_network_id,
                    msg.transport_stream_id,
                    msg.service_id,
                    msg.event.event_id);
                self.handle_event_update(quad, msg.event, msg.record);
            }
            TimeshiftRecorderMessage::EventEnd(msg) => {
                let quad = EventQuad::new(
                    msg.original_network_id,
                    msg.transport_stream_id,
                    msg.service_id,
                    msg.event.event_id);
                self.handle_event_end(quad, msg.event, msg.record);
            }
        }
    }

    fn finished(&mut self, _ctx: &mut actix::Context<Self>) {
        log::info!("{}: Recoding pipeline broken, deactivate", self.name);
        self.deactivate();
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
#[serde(tag = "type", content = "data")]
enum TimeshiftRecorderMessage {
    Start,
    Stop(TimeshiftRecorderStopMessage),
    Chunk(TimeshiftRecorderChunkMessage),
    EventStart(TimeshiftRecorderEventMessage),
    EventUpdate(TimeshiftRecorderEventMessage),
    EventEnd(TimeshiftRecorderEventMessage),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TimeshiftRecorderStartMessage;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TimeshiftRecorderStopMessage {
    reset: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TimeshiftRecorderChunkMessage {
    chunk: TimeshiftPoint,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TimeshiftRecorderEventMessage {
    original_network_id: NetworkId,
    transport_stream_id: TransportStreamId,
    service_id: ServiceId,
    event: EitEvent,
    record: TimeshiftPoint,
}

#[derive(Clone)]
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TimeshiftPoint {
    #[serde(with = "serde_jst")]
    timestamp: DateTime<Jst>,
    pos: u64,
}

impl fmt::Display for TimeshiftPoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.timestamp, self.pos)
    }
}

struct TimeshiftRecorderSession {
    pipeline: CommandPipeline<TunerSubscriptionId>,
    _stop_trigger: TunerStreamStopTrigger,
}

struct TimeshiftActivation {
    config: Arc<Config>,
    name: String,
    service: EpgService,
    tuner_manager: Addr<TunerManager>,
}

struct TimeshiftActivationResult {
    session: TimeshiftRecorderSession,
    output: CommandPipelineOutput<TunerSubscriptionId>,
}

#[derive(Clone)]
struct TimeshiftRecord {
    id: TimeshiftRecordId,
    program: EpgProgram,
    start: TimeshiftPoint,
    end: TimeshiftPoint,
    recording: bool,
}

impl TimeshiftRecord {
    fn new(id: TimeshiftRecordId, program: EpgProgram, point: TimeshiftPoint) -> Self {
        TimeshiftRecord {
            id,
            program,
            start: point.clone(),
            end: point.clone(),
            recording: true,
        }
    }

    fn update(&mut self, program: EpgProgram, point: TimeshiftPoint, end: bool) {
        self.program = program;
        self.end = point;
        if end {
            self.recording = false;
        }
    }

    fn create_record_stream_source(
        &self,
        recorder_name: String,
        config: &TimeshiftConfig,
        start_pos: Option<u64>,
    ) -> Result<TimeshiftRecordStreamSource, Error> {
        let file = config.file.clone();
        let file_size = config.max_file_size();
        let id = self.id;
        let size = self.get_size(file_size);
        let (start, range) = if let Some(pos) = start_pos {
            ((self.start.pos + pos) % file_size, self.make_range(pos, size)?)
        } else {
            (self.start.pos, self.make_range(0, size)?)
        };
        Ok(TimeshiftRecordStreamSource { recorder_name, file, id, start, range })
    }

    fn get_model(&self, config: &TimeshiftConfig) -> TimeshiftRecordModel {
        TimeshiftRecordModel {
            id: self.id,
            program: self.program.clone(),
            start_time: self.start.timestamp.clone(),
            end_time: self.end.timestamp.clone(),
            size: self.get_size(config.max_file_size()),
            recording: self.recording,
        }
    }

    fn get_size(&self, file_size: u64) -> u64 {
        if self.end.pos < self.start.pos {
            file_size - self.start.pos + self.end.pos
        } else {
            self.end.pos - self.start.pos
        }
    }

    fn make_range(&self, first: u64, size: u64) -> Result<MpegTsStreamRange, Error> {
        if self.recording {
            MpegTsStreamRange::unbound(first, size)
        } else {
            MpegTsStreamRange::bound(first, size)
        }
    }
}

// 32 KiB, large enough for 10 ms buffering.
const CHUNK_SIZE: usize = 4096 * 8;

struct TimeshiftStreamSource {
    name: String,
    file: String,
    point: TimeshiftPoint,
}

impl TimeshiftStreamSource {
    async fn create_stream(
        self
    ) -> Result<(TimeshiftStream, TimeshiftStreamStopTrigger), Error> {
        log::debug!("{}: Start live streaming from {}", self.name, self.point);
        let (mut reader, stop_trigger) = TimeshiftFileReader::open(&self.file).await?;
        reader.set_position(self.point.pos).await?;
        let stream = ChunkStream::new(reader, CHUNK_SIZE);
        let id = format!("timeshift({})", self.name);
        Ok((MpegTsStream::new(id, stream).decoded(), stop_trigger))
    }
}

struct TimeshiftRecordStreamSource {
    recorder_name: String,
    file: String,
    id: TimeshiftRecordId,
    start: u64,
    range: MpegTsStreamRange,
}

impl TimeshiftRecordStreamSource {
    async fn create_stream(
        self
    ) -> Result<(TimeshiftRecordStream, TimeshiftStreamStopTrigger), Error> {
        log::debug!("{}: Start streaming {} bytes of Record#{} from {}",
                    self.recorder_name, self.range.bytes(), self.id, self.start);
        let (mut reader, stop_trigger) = TimeshiftFileReader::open(&self.file).await?;
        reader.set_position(self.start).await?;
        let stream = ChunkStream::new(reader.take(self.range.bytes()), CHUNK_SIZE);
        let id = format!("timeshift({})/record#{}", self.recorder_name, self.id);
        Ok((MpegTsStream::with_range(id, stream, self.range).decoded(), stop_trigger))
    }
}

pub struct TimeshiftFileReader {
    state: TimeshiftFileReaderState,
    path: String,
    file: File,
    stop_signal: oneshot::Receiver<()>,
}

enum TimeshiftFileReaderState {
    Read,
    Seek,
    Wait,
}

impl TimeshiftFileReader {
    async fn open(path: &str) -> Result<(Self, TimeshiftStreamStopTrigger), Error> {
        let (tx, rx) = oneshot::channel();
        let reader = TimeshiftFileReader {
            state: TimeshiftFileReaderState::Read,
            path: path.to_string(),
            file: File::open(path).await?,
            stop_signal: rx,
        };
        let stop_trigger = TimeshiftStreamStopTrigger(Some(tx));
        Ok((reader, stop_trigger))
    }

    async fn set_position(&mut self, pos: u64) -> Result<(), Error> {
        let _ = self.file.seek(SeekFrom::Start(pos)).await;
        Ok(())
    }

    #[cfg(test)]
    pub fn open_for_test() -> (Self, TimeshiftStreamStopTrigger) {
        let (tx, rx) = oneshot::channel();
        let reader = TimeshiftFileReader {
            state: TimeshiftFileReaderState::Read,
            path: "/dev/zero".to_string(),
            file: std::fs::File::open("/dev/zero").unwrap().into(),
            stop_signal: rx,
        };
        let stop_trigger = TimeshiftStreamStopTrigger(Some(tx));
        (reader, stop_trigger)
    }
}

impl AsyncRead for TimeshiftFileReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        loop {
            if Pin::new(&mut self.stop_signal).poll(cx).is_ready() {
                log::debug!("{}: Stopped reading", self.path);
                return Poll::Ready(Ok(0));
            }
            match self.state {
                TimeshiftFileReaderState::Read => {
                    match Pin::new(&mut self.file).poll_read(cx, buf) {
                        Poll::Ready(Ok(0)) => {
                            self.state = TimeshiftFileReaderState::Seek;
                            log::debug!("{}: EOF reached", self.path);
                        }
                        poll => {
                            return poll;
                        }
                    }
                }
                TimeshiftFileReaderState::Seek => {
                    match Pin::new(&mut self.file).start_seek(cx, SeekFrom::Start(0)) {
                        Poll::Ready(Ok(_)) => {
                            self.state = TimeshiftFileReaderState::Wait;
                            log::debug!("{}: Seek to the beginning", self.path);
                        }
                        Poll::Ready(Err(err)) => {
                            return Poll::Ready(Err(err));
                        }
                        Poll::Pending => {
                            return Poll::Pending;
                        }
                    }
                }
                TimeshiftFileReaderState::Wait => {
                    match Pin::new(&mut self.file).poll_complete(cx) {
                        Poll::Ready(Ok(pos)) => {
                            assert!(pos == 0);
                            self.state = TimeshiftFileReaderState::Read;
                            log::debug!("{}: The seek completed, restart streaming", self.path);
                        }
                        Poll::Ready(Err(err)) => {
                            return Poll::Ready(Err(err));
                        }
                        Poll::Pending => {
                            return Poll::Pending;
                        }
                    }
                }
            }
        }
    }
}

pub struct TimeshiftStreamStopTrigger(Option<oneshot::Sender<()>>);

impl Drop for TimeshiftStreamStopTrigger {
    fn drop(&mut self) {
        let _ = self.0.take().unwrap().send(());
    }
}

pub struct TimeshiftRecorderModel {
    pub name: String,
    pub service: EpgService,
    pub start_time: DateTime<Jst>,
    pub end_time: DateTime<Jst>,
    pub pipeline: Vec<CommandPipelineProcessModel>,
    pub recording: bool,
}

pub struct TimeshiftRecordModel {
    pub id: TimeshiftRecordId,
    pub program: EpgProgram,
    pub start_time: DateTime<Jst>,
    pub end_time: DateTime<Jst>,
    pub size: u64,
    pub recording: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use crate::datetime_ext::Jst;

    #[test]
    fn test_timeshift_record_purge_expired_records() {
        let mut recorder = TimeshiftRecorder {
            name: "record".to_string(),
            config: create_config(),
            service: create_epg_service(),
            records: indexmap::indexmap!{
                0.into() => TimeshiftRecord {
                    id: 0.into(),
                    program: EpgProgram::new((0, 0, 0, 1).into()),
                    start: TimeshiftPoint {
                        timestamp: Jst.ymd(2021, 1, 1).and_hms(0, 0, 0),
                        pos: 0,
                    },
                    end: TimeshiftPoint {
                        timestamp: Jst.ymd(2021, 1, 1).and_hms(0, 0, 0),
                        pos: 0,
                    },
                    recording: false,
                },
            },
            points: vec![
                TimeshiftPoint {
                    timestamp: Jst.ymd(2021, 1, 1).and_hms(0, 1, 0),
                    pos: 0,
                },
            ],
            session: None,
        };
        recorder.purge_expired_records();
        assert!(recorder.records.is_empty());

        let mut recorder = TimeshiftRecorder {
            name: "recorder".to_string(),
            config: create_config(),
            service: create_epg_service(),
            records: indexmap::indexmap!{
                0.into() => TimeshiftRecord {
                    id: 0.into(),
                    program: EpgProgram::new((0, 0, 0, 1).into()),
                    start: TimeshiftPoint {
                        timestamp: Jst.ymd(2021, 1, 1).and_hms(0, 0, 0),
                        pos: 0,
                    },
                    end: TimeshiftPoint {
                        timestamp: Jst.ymd(2021, 1, 1).and_hms(0, 0, 0),
                        pos: 0,
                    },
                    recording: false,
                },
                1.into() => TimeshiftRecord {
                    id: 1.into(),
                    program: EpgProgram::new((0, 0, 0, 2).into()),
                    start: TimeshiftPoint {
                        timestamp: Jst.ymd(2021, 1, 1).and_hms(0, 0, 0),
                        pos: 0,
                    },
                    end: TimeshiftPoint {
                        timestamp: Jst.ymd(2021, 1, 1).and_hms(0, 1, 0),
                        pos: 0,
                    },
                    recording: false,
                },
                2.into() => TimeshiftRecord {
                    id: 2.into(),
                    program: EpgProgram::new((0, 0, 0, 3).into()),
                    start: TimeshiftPoint {
                        timestamp: Jst.ymd(2021, 1, 1).and_hms(0, 0, 0),
                        pos: 0,
                    },
                    end: TimeshiftPoint {
                        timestamp: Jst.ymd(2021, 1, 1).and_hms(0, 2, 0),
                        pos: 0,
                    },
                    recording: false,
                },
            },
            points: vec![
                TimeshiftPoint {
                    timestamp: Jst.ymd(2021, 1, 1).and_hms(0, 1, 0),
                    pos: 0,
                },
            ],
            session: None,
        };
        recorder.purge_expired_records();
        assert_eq!(recorder.records.len(), 1);
        assert_eq!(recorder.records[0].program.quad, (0, 0, 0, 3).into());
    }

    fn create_config() -> Arc<Config> {
        Default::default()
    }

    fn create_epg_service() -> EpgService {
        EpgService {
            nid: 1.into(),
            tsid: 2.into(),
            sid: 3.into(),
            service_type: 1,
            logo_id: 0,
            remote_control_key_id: 0,
            name: "Service".to_string(),
            channel:  EpgChannel {
                name: "ch".to_string(),
                channel_type: ChannelType::GR,
                channel: "ch".to_string(),
                extra_args: "".to_string(),
                services: vec![],
                excluded_services: vec![],
            }
        }
    }
}
