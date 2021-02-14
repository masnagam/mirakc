use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use actix::prelude::*;
use chrono::DateTime;
use serde::Deserialize;
use tokio::prelude::*;
use tokio::io::BufReader;

use crate::config::{Config, TimeshiftConfig};
use crate::datetime_ext::*;
use crate::eit_feeder::*;
use crate::error::Error;
use crate::epg::*;
use crate::filter::*;
use crate::models::*;
use crate::mpeg_ts_stream::*;
use crate::tuner::*;
use crate::command_util::spawn_pipeline;

pub fn start(
    config: Arc<Config>,
    tuner_manager: Addr<TunerManager>,
) -> Addr<TimeshiftManager> {
    TimeshiftManager::new(config, tuner_manager).start()
}

// timeshift manager

pub struct TimeshiftManager {
    config: Arc<Config>,
    tuner_manager: Addr<TunerManager>,
    records: HashMap<String, TimeshiftRecord>,
}

impl TimeshiftManager {
    pub fn new(config: Arc<Config>, tuner_manager: Addr<TunerManager>) -> Self {
        let records = HashMap::new();
        TimeshiftManager { config, tuner_manager, records, }
    }

    fn start_streaming(
        &self,
        record_id: &str,
        program_id: Option<MirakurunProgramId>,
    ) -> Result<MpegTsStream, Error> {
        let record = self.records.get(record_id).ok_or(Error::RecordNotFound)?;
        record.start_streaming(program_id)
    }

    fn start_recording(&mut self, name: &str) {
        self.records.get_mut(name).unwrap().start_recording();
    }

    fn stop_recording(&mut self, name: &str, reset: bool) {
        self.records.get_mut(name).unwrap().stop_recording(reset);
    }

    fn update_chunk_timestamp(&mut self, name: &str, point: TimeshiftPoint) {
        self.records.get_mut(name).unwrap().update_chunk(point);
    }

    fn start_program(
        &mut self,
        name: &str,
        quad: EventQuad,
        event: EitEvent,
        point: TimeshiftPoint,
    ) {
        self.records.get_mut(name).unwrap().start_program(quad, event, point);
    }

    fn end_program(
        &mut self,
        name: &str,
        quad: EventQuad,
        event: EitEvent,
        point: TimeshiftPoint,
    ) {
        self.records.get_mut(name).unwrap().end_program(quad, event, point);
    }

    async fn activate_recorders(
        config: Arc<Config>,
        tuner_manager: Addr<TunerManager>,
        manager: Addr<TimeshiftManager>,
    ) -> Vec<TimeshiftRecorder> {
        let mut recorders = Vec::new();
        for name in config.timeshift.keys() {
            match Self::activate_recorder(name, &config, &tuner_manager, manager.clone()).await {
                Ok(recorder) => {
                    log::info!("Activated a timeshift recorder for {}", name);
                    recorders.push(recorder);
                }
                Err(err) => {
                    log::error!("Failed to activatea timeshift recorder for {}: {}", name, err);
                }
            }
        }
        recorders
    }

    async fn activate_recorder(
        name: &str,
        config: &Arc<Config>,
        tuner_manager: &Addr<TunerManager>,
        manager: Addr<TimeshiftManager>,
    ) -> Result<TimeshiftRecorder, Error> {
        let timeshift_config = config.timeshift.get(name).unwrap();

        let channel = config.channels.iter()
            .filter(|config| !config.disabled)
            .find(|config| config.name == name)
            .cloned()
            .map(EpgChannel::from)
            .ok_or(Error::ChannelNotFound)?;

        let user = TunerUser {
            info: TunerUserInfo::Recorder {
                name: format!("timeshift({})", name),
            },
            priority: timeshift_config.priority.into(),
        };

        let data = mustache::MapBuilder::new()
            .insert_str("channel_name", &channel.name)
            .insert("channel_type", &channel.channel_type)?
            .insert_str("channel", &channel.channel)
            .insert("sid", &timeshift_config.sid)?
            .build();
        let mut builder = FilterPipelineBuilder::new(data);
        builder.add_service_filter(&config.filters.service_filter)?;
        builder.add_decode_filter(&config.filters.decode_filter)?;
        let (mut cmds, _) = builder.build();

        let data = mustache::MapBuilder::new()
            .insert_str("name", name)
            .insert("sid", &timeshift_config.sid)?
            .insert_str("file", &timeshift_config.file)
            .insert("chunk_size", &timeshift_config.chunk_size)?
            .insert("num_chunks", &timeshift_config.num_chunks)?
            .build();
        let template = mustache::compile_str(&config.recorder.record_service_command)?;
        cmds.push(template.render_data_to_string(&data)?);

        let mut stream = tuner_manager.send(StartStreamingMessage {
            channel: channel.clone(),
            user,
        }).await??;

        let stop_trigger = stream.take_stop_trigger();

        let mut pipeline = spawn_pipeline(cmds, stream.id())?;

        let (input, output) = pipeline.take_endpoints()?;

        actix::spawn(async move {
            let _ = stream.pipe(input).await;
            drop(pipeline);
        });

        actix::spawn(async move {
            match Self::forward_messages(manager, output).await {
                Ok(_) => (),
                Err(err) => log::error!("{}", err),
            }
            drop(stop_trigger);  // TODO: stop_trigger
        });

        Ok(TimeshiftRecorder {
            name: name.to_string(),
            config: timeshift_config.clone(),
            channel,
            // TODO: stop_trigger
        })
    }

    async fn forward_messages<T: AsyncRead + Unpin>(
        manager: Addr<TimeshiftManager>,
        output: T,
    ) -> Result<(), Error> {
        let mut reader = BufReader::new(output);
        let mut json = String::new();
        while reader.read_line(&mut json).await? > 0 {
            let msg = serde_json::from_str::<TimeshiftMessage>(&json)?;
            manager.do_send(msg);
            json.clear();
        }
        Ok(())
    }
}

impl Actor for TimeshiftManager {
    type Context = actix::Context<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        log::debug!("Started");
        let fut = Self::activate_recorders(
            self.config.clone(), self.tuner_manager.clone(), ctx.address());
        actix::fut::wrap_future::<_, Self>(fut)
            .then(|recorders, actor, _ctx| {
                for recorder in recorders.into_iter() {
                    let name = recorder.name.clone();
                    let record = TimeshiftRecord::new(
                        recorder.name, recorder.config, recorder.channel);
                    // TODO: copy stop_triggers
                    actor.records.insert(name, record);
                }
                actix::fut::ready(())
            })
            .wait(ctx);
    }

    fn stopped(&mut self, _: &mut Self::Context) {
        log::debug!("Stopped");
    }
}

#[derive(Message)]
#[rtype(result = "Result<Vec<TimeshiftRecordModel>, Error>")]
pub struct QueryTimeshiftRecordsMessage;

impl fmt::Display for QueryTimeshiftRecordsMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "QueryTimeshiftRecords")
    }
}

impl Handler<QueryTimeshiftRecordsMessage> for TimeshiftManager {
    type Result = MessageResult<QueryTimeshiftRecordsMessage>;

    fn handle(
        &mut self,
        msg: QueryTimeshiftRecordsMessage,
        _: &mut Self::Context,
    ) -> Self::Result {
        log::debug!("{}", msg);
        let models: Vec<TimeshiftRecordModel> = self.records.values()
            .map(|record| record.get_model())
            .collect();
        MessageResult(Ok(models))
    }
}

#[derive(Message)]
#[rtype(result = "Result<TimeshiftRecordModel, Error>")]
pub struct QueryTimeshiftRecordMessage {
    pub record: String,
}

impl fmt::Display for QueryTimeshiftRecordMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "QueryTimeshiftRecord for {}", self.record)
    }
}

impl Handler<QueryTimeshiftRecordMessage> for TimeshiftManager {
    type Result = MessageResult<QueryTimeshiftRecordMessage>;

    fn handle(
        &mut self,
        msg: QueryTimeshiftRecordMessage,
        _: &mut Self::Context,
    ) -> Self::Result {
        log::debug!("{}", msg);
        let result = self.records.get(&msg.record)
            .map(|record| record.get_model())
            .ok_or(Error::RecordNotFound);
        MessageResult(result)
    }
}

#[derive(Message)]
#[rtype(result = "Result<Vec<EpgProgram>, Error>")]
pub struct QueryTimeshiftProgramsMessage {
    pub record: String,
}

impl fmt::Display for QueryTimeshiftProgramsMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "QueryTimeshiftPrograms for {}", self.record)
    }
}

impl Handler<QueryTimeshiftProgramsMessage> for TimeshiftManager {
    type Result = MessageResult<QueryTimeshiftProgramsMessage>;

    fn handle(
        &mut self,
        msg: QueryTimeshiftProgramsMessage,
        _: &mut Self::Context,
    ) -> Self::Result {
        log::debug!("{}", msg);
        let result = self.records.get(&msg.record)
            .map(|record| {
                record.programs.iter()
                    .map(|program| program.epg.clone())
                    .collect::<Vec<EpgProgram>>()
            })
            .ok_or(Error::RecordNotFound);
        MessageResult(result)
    }
}

#[derive(Message)]
#[rtype(result = "Result<MpegTsStream, Error>")]
pub struct StartTimeshiftStreamingMessage {
    pub record: String,
    pub program: Option<MirakurunProgramId>,
}

impl fmt::Display for StartTimeshiftStreamingMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "StartTimeshiftStreaming for {}", self.record)
    }
}

impl Handler<StartTimeshiftStreamingMessage> for TimeshiftManager {
    type Result = MessageResult<StartTimeshiftStreamingMessage>;

    fn handle(
        &mut self,
        msg: StartTimeshiftStreamingMessage,
        _: &mut Self::Context,
    ) -> Self::Result {
        log::debug!("{}", msg);
        MessageResult(self.start_streaming(&msg.record, msg.program))
    }
}

#[derive(Message)]
#[rtype(result = "()")]
#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
#[serde(tag = "type", content = "data")]
enum TimeshiftMessage {
    Start(TimeshiftStartMessage),
    Stop(TimeshiftStopMessage),
    ChunkTimestamp(TimeshiftChunkTimestampMessage),
    StartProgram(TimeshiftProgramMessage),
    EndProgram(TimeshiftProgramMessage),
}

impl Handler<TimeshiftMessage> for TimeshiftManager {
    type Result = MessageResult<TimeshiftMessage>;

    fn handle(&mut self, msg: TimeshiftMessage, _: &mut Self::Context) -> Self::Result {
        match msg {
            TimeshiftMessage::Start(msg) => self.start_recording(&msg.id),
            TimeshiftMessage::Stop(msg) => self.stop_recording(&msg.id, msg.reset),
            TimeshiftMessage::ChunkTimestamp(msg) => {
                self.update_chunk_timestamp(&msg.id, msg.chunk);
            }
            TimeshiftMessage::StartProgram(msg) => {
                let quad = EventQuad::new(
                    msg.original_network_id,
                    msg.transport_stream_id,
                    msg.service_id,
                    msg.event.event_id);
                self.start_program(&msg.id, quad, msg.event, msg.record);
            }
            TimeshiftMessage::EndProgram(msg) => {
                let quad = EventQuad::new(
                    msg.original_network_id,
                    msg.transport_stream_id,
                    msg.service_id,
                    msg.event.event_id);
                self.end_program(&msg.id, quad, msg.event, msg.record);
            }
        }
        MessageResult(())
    }
}

struct TimeshiftRecorder {
    name: String,
    config: TimeshiftConfig,
    channel: EpgChannel,
}

struct TimeshiftRecord {
    name: String,
    config: TimeshiftConfig,
    channel: EpgChannel,
    programs: Vec<TimeshiftProgram>,
    points: Vec<TimeshiftPoint>,
    recording: bool,
}

impl TimeshiftRecord {
    fn new(name: String, config: TimeshiftConfig, channel: EpgChannel) -> Self {
        let max_chunks = config.max_chunks();
        TimeshiftRecord {
            name,
            config,
            channel,
            programs: Vec::new(),
            points: Vec::with_capacity(max_chunks),
            recording: false,
        }
    }

    fn start_streaming(
        &self,
        program_id: Option<MirakurunProgramId>,
    ) -> Result<MpegTsStream, Error> {
        let point = match program_id {
            Some(id) => {
                &self.programs.iter()
                    .find(|program| id == program.epg.quad.into())
                    .ok_or(Error::ProgramNotFound)?
                    .start
            }
            None => {
                self.points.iter()
                    .next()
                    .ok_or(Error::RecordNotFound)?
            }
        };
        log::info!("{}: Start streaming from {}@{}", self.name, point.timestamp, point.pos);
        // TODO
        // 32 KiB, large enough for 10 ms buffering.
        //const CHUNK_SIZE: usize = 4096 * 8;
        //let file = File::open(&self.config.file).await?;
        //let reader = ChunkStream::new(file, CHUNK_SIZE);
        Err(Error::ProgramNotFound)
    }

    fn start_recording(&mut self) {
        self.recording = true;
        log::info!("{}: Started recording", self.name);
    }

    fn stop_recording(&mut self, reset: bool) {
        self.recording = false;
        log::info!("{}: Stopped recording", self.name);
        if reset {
            log::warn!("{}: Reset data", self.name);
        }
    }

    fn update_chunk(&mut self, point: TimeshiftPoint) {
        self.maintain();
        let index = point.pos / self.config.chunk_size;
        assert!(point.pos % self.config.chunk_size == 0);
        log::info!("{}: Chunk#{}.timestamp: {}", self.name, index, point.timestamp);
        self.points.push(point);
        assert!(self.points.len() <= self.config.max_chunks());
    }

    fn maintain(&mut self) {
        if self.points.len() < self.config.max_chunks() {
            return;
        }
        self.invalidate_first_chunk();
        self.expire_programs();
    }

    fn invalidate_first_chunk(&mut self) {
        assert!(self.points.len() == self.config.max_chunks());
        let point = self.points.remove(0);
        let index = point.pos / self.config.chunk_size;
        log::debug!("{}: Invalidated Chunk#{}", self.name, index);
    }

    fn expire_programs(&mut self) {
        assert!(!self.points.is_empty());
        let timestamp = self.points[0].timestamp;  // timestamp of the first chunk
        let index = self.programs.iter()
            .position(|program| program.end.timestamp > timestamp);
        if let Some(n) = index {
            for program in self.programs.drain(0..n) {  // remove first n programs
                log::info!("{}: Program#{} ({}) expired",
                           self.name, program.epg.quad, program.epg.name());
            }
        }
    }

    fn start_program(
        &mut self,
        quad: EventQuad,
        event: EitEvent,
        point: TimeshiftPoint,
    ) {
        let mut epg = EpgProgram::new(quad);
        epg.update(&event);
        log::info!("{}: Program#{} ({}) started", self.name, quad, epg.name());
        self.programs.push(TimeshiftProgram {
            epg,
            start: point.clone(),
            end: point.clone(),
        });
    }

    fn end_program(
        &mut self,
        quad: EventQuad,
        event: EitEvent,
        point: TimeshiftPoint,
    ) {
        let mut epg = EpgProgram::new(quad);
        epg.update(&event);
        log::info!("{}: Program#{} ({}) ended", self.name, quad, epg.name());
        let last = self.programs.iter_mut()
            .last()
            .filter(|program| program.epg.quad == quad);
        if let Some(mut program) = last {
            program.epg = epg;
            program.end = point;
        }
    }

    fn get_model(&self) -> TimeshiftRecordModel {
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
        TimeshiftRecordModel {
            name: self.name.clone(),
            start_time,
            end_time,
            recording: self.recording,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TimeshiftStartMessage {
    id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TimeshiftStopMessage {
    id: String,
    reset: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TimeshiftChunkTimestampMessage {
    id: String,
    chunk: TimeshiftPoint,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TimeshiftProgramMessage {
    id: String,
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
    pos: usize,
}

#[derive(Clone)]
struct TimeshiftProgram {
    epg: EpgProgram,
    start: TimeshiftPoint,
    end: TimeshiftPoint,
}
