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
use crate::command_util::spawn_pipeline;

pub fn start(
    config: Arc<Config>,
    tuner_manager: Addr<TunerManager>,
) -> Addr<TimeshiftManager> {
    TimeshiftManager::new(config, tuner_manager).start()
}

// timeshift manager

type TimeshiftStream = MpegTsStream<usize, ChunkStream<TimeshiftFile>>;
type TimeshiftRecordStream = MpegTsStream<usize, ChunkStream<Take<TimeshiftFile>>>;

pub struct TimeshiftManager {
    config: Arc<Config>,
    tuner_manager: Addr<TunerManager>,
    recorders: HashMap<String, TimeshiftRecorder>,
}

impl TimeshiftManager {
    pub fn new(config: Arc<Config>, tuner_manager: Addr<TunerManager>) -> Self {
        let recorders = HashMap::new();
        TimeshiftManager { config, tuner_manager, recorders, }
    }

    fn create_live_stream_source(
        &self,
        recorder_name: &str,
        record_id: Option<TimeshiftRecordId>,
    ) -> Result<TimeshiftStreamSource, Error> {
        let recorder = self.recorders.get(recorder_name).ok_or(Error::RecordNotFound)?;
        recorder.create_live_stream_source(record_id)
    }

    fn create_record_stream_source(
        &self,
        recorder_name: &str,
        record_id: TimeshiftRecordId,
    ) -> Result<TimeshiftRecordStreamSource, Error> {
        let recorder = self.recorders.get(recorder_name).ok_or(Error::RecordNotFound)?;
        recorder.create_record_stream_source(record_id)
    }

    fn start_recording(&mut self, name: &str) {
        self.recorders.get_mut(name).unwrap().start_recording();
    }

    fn stop_recording(&mut self, name: &str, reset: bool) {
        self.recorders.get_mut(name).unwrap().stop_recording(reset);
    }

    fn handle_chunk(&mut self, name: &str, point: TimeshiftPoint) {
        self.recorders.get_mut(name).unwrap().handle_chunk(point);
    }

    fn handle_event_start(
        &mut self,
        name: &str,
        quad: EventQuad,
        event: EitEvent,
        point: TimeshiftPoint,
    ) {
        self.recorders.get_mut(name).unwrap().handle_event_start(quad, event, point);
    }

    fn handle_event_update(
        &mut self,
        name: &str,
        quad: EventQuad,
        event: EitEvent,
        point: TimeshiftPoint,
    ) {
        self.recorders.get_mut(name).unwrap().handle_event_update(quad, event, point);
    }

    fn handle_event_end(
        &mut self,
        name: &str,
        quad: EventQuad,
        event: EitEvent,
        point: TimeshiftPoint,
    ) {
        self.recorders.get_mut(name).unwrap().handle_event_end(quad, event, point);
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
                    log::debug!("Activated a timeshift recorder for {}", name);
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
            .find(|config| config.name == timeshift_config.channel)
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

        let stream = tuner_manager.send(StartStreamingMessage {
            channel: channel.clone(),
            user,
        }).await??;

        let stop_trigger = TunerStreamStopTrigger::new(
            stream.id(), tuner_manager.clone().recipient());

        let mut pipeline = spawn_pipeline(cmds, stream.id())?;

        let (input, output) = pipeline.take_endpoints()?;

        actix::spawn(async move {
            let _ = stream.pipe(input).await;
            drop(pipeline);
            // TODO: respawn the recorder if it stopped due to an error.
        });

        actix::spawn(async move {
            match Self::forward_messages(manager, output).await {
                Ok(_) => (),
                Err(err) => log::error!("{}", err),
            }
            drop(stop_trigger);  // TODO: stop_trigger
        });

        // TODO: stop_trigger
        Ok(TimeshiftRecorder::new(name.to_string(), timeshift_config.clone(), channel))
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
                    actor.recorders.insert(name, recorder);
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
#[rtype(result = "Result<Vec<TimeshiftRecorderModel>, Error>")]
pub struct QueryTimeshiftRecordersMessage;

impl fmt::Display for QueryTimeshiftRecordersMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "QueryTimeshiftRecorders")
    }
}

impl Handler<QueryTimeshiftRecordersMessage> for TimeshiftManager {
    type Result = MessageResult<QueryTimeshiftRecordersMessage>;

    fn handle(
        &mut self,
        msg: QueryTimeshiftRecordersMessage,
        _: &mut Self::Context,
    ) -> Self::Result {
        log::debug!("{}", msg);
        let models: Vec<TimeshiftRecorderModel> = self.recorders.values()
            .map(|recorder| recorder.get_model())
            .collect();
        MessageResult(Ok(models))
    }
}

#[derive(Message)]
#[rtype(result = "Result<TimeshiftRecorderModel, Error>")]
pub struct QueryTimeshiftRecorderMessage {
    pub name: String,
}

impl fmt::Display for QueryTimeshiftRecorderMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "QueryTimeshiftRecorder for {}", self.name)
    }
}

impl Handler<QueryTimeshiftRecorderMessage> for TimeshiftManager {
    type Result = MessageResult<QueryTimeshiftRecorderMessage>;

    fn handle(
        &mut self,
        msg: QueryTimeshiftRecorderMessage,
        _: &mut Self::Context,
    ) -> Self::Result {
        log::debug!("{}", msg);
        let model = self.recorders.get(&msg.name)
            .map(|recorder| recorder.get_model())
            .ok_or(Error::RecordNotFound);
        MessageResult(model)
    }
}

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

impl Handler<QueryTimeshiftRecordsMessage> for TimeshiftManager {
    type Result = MessageResult<QueryTimeshiftRecordsMessage>;

    fn handle(
        &mut self,
        msg: QueryTimeshiftRecordsMessage,
        _: &mut Self::Context,
    ) -> Self::Result {
        log::debug!("{}", msg);
        let models = self.recorders.get(&msg.recorder_name)
            .map(|recorder| {
                recorder.records.values()
                    .map(|record| record.get_model(&recorder.config))
                    .collect::<Vec<TimeshiftRecordModel>>()
            })
            .ok_or(Error::RecordNotFound);
        MessageResult(models)
    }
}


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

impl Handler<QueryTimeshiftRecordMessage> for TimeshiftManager {
    type Result = MessageResult<QueryTimeshiftRecordMessage>;

    fn handle(
        &mut self,
        msg: QueryTimeshiftRecordMessage,
        _: &mut Self::Context,
    ) -> Self::Result {
        log::debug!("{}", msg);
        let result = self.recorders.get(&msg.recorder_name)
            .iter()
            .flat_map(|recorder| {
                recorder.records
                    .get(&msg.record_id)
                    .map(|record| record.get_model(&recorder.config))
            })
            .next()
            .ok_or(Error::RecordNotFound);
        MessageResult(result)
    }
}

#[derive(Message)]
#[rtype(result = "Result<TimeshiftStream, Error>")]
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

impl Handler<StartTimeshiftStreamingMessage> for TimeshiftManager {
    type Result = ResponseFuture<Result<TimeshiftStream, Error>>;

    fn handle(
        &mut self,
        msg: StartTimeshiftStreamingMessage,
        _: &mut Self::Context,
    ) -> Self::Result {
        log::debug!("{}", msg);
        let src = self.create_live_stream_source(&msg.recorder_name, msg.record_id);
        Box::pin(async move {
            src?.create_stream().await
        })
    }
}

#[derive(Message)]
#[rtype(result = "Result<TimeshiftRecordStream, Error>")]
pub struct StartTimeshiftRecordStreamingMessage {
    pub recorder_name: String,
    pub record_id: TimeshiftRecordId,
}

impl fmt::Display for StartTimeshiftRecordStreamingMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "StartTimeshiftRecordStreaming for Record#{} in {}",
               self.record_id, self.recorder_name)
    }
}

impl Handler<StartTimeshiftRecordStreamingMessage> for TimeshiftManager {
    type Result = ResponseFuture<Result<TimeshiftRecordStream, Error>>;

    fn handle(
        &mut self,
        msg: StartTimeshiftRecordStreamingMessage,
        _: &mut Self::Context,
    ) -> Self::Result {
        log::debug!("{}", msg);
        let src = self.create_record_stream_source(&msg.recorder_name, msg.record_id);
        Box::pin(async move {
            src?.create_stream().await
        })
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
    Chunk(TimeshiftChunkMessage),
    EventStart(TimeshiftEventMessage),
    EventUpdate(TimeshiftEventMessage),
    EventEnd(TimeshiftEventMessage),
}

impl Handler<TimeshiftMessage> for TimeshiftManager {
    type Result = MessageResult<TimeshiftMessage>;

    fn handle(&mut self, msg: TimeshiftMessage, _: &mut Self::Context) -> Self::Result {
        match msg {
            TimeshiftMessage::Start(msg) => {
                self.start_recording(&msg.id);
            }
            TimeshiftMessage::Stop(msg) => {
                self.stop_recording(&msg.id, msg.reset);
            }
            TimeshiftMessage::Chunk(msg) => {
                self.handle_chunk(&msg.id, msg.chunk);
            }
            TimeshiftMessage::EventStart(msg) => {
                let quad = EventQuad::new(
                    msg.original_network_id,
                    msg.transport_stream_id,
                    msg.service_id,
                    msg.event.event_id);
                self.handle_event_start(&msg.id, quad, msg.event, msg.record);
            }
            TimeshiftMessage::EventUpdate(msg) => {
                let quad = EventQuad::new(
                    msg.original_network_id,
                    msg.transport_stream_id,
                    msg.service_id,
                    msg.event.event_id);
                self.handle_event_update(&msg.id, quad, msg.event, msg.record);
            }
            TimeshiftMessage::EventEnd(msg) => {
                let quad = EventQuad::new(
                    msg.original_network_id,
                    msg.transport_stream_id,
                    msg.service_id,
                    msg.event.event_id);
                self.handle_event_end(&msg.id, quad, msg.event, msg.record);
            }
        }
        MessageResult(())
    }
}

struct TimeshiftRecorder {
    name: String,
    config: TimeshiftConfig,
    channel: EpgChannel,
    records: IndexMap<TimeshiftRecordId, TimeshiftRecord>,
    points: Vec<TimeshiftPoint>,
    next_record_id: usize,
    recording: bool,
}

impl TimeshiftRecorder {
    fn new(name: String, config: TimeshiftConfig, channel: EpgChannel) -> Self {
        let max_chunks = config.max_chunks();
        TimeshiftRecorder {
            name,
            config,
            channel,
            records: IndexMap::new(),
            points: Vec::with_capacity(max_chunks),
            next_record_id: 0,
            recording: false,
        }
    }

    fn create_live_stream_source(
        &self,
        record_id: Option<TimeshiftRecordId>,
    ) -> Result<TimeshiftStreamSource, Error> {
        if self.points.len() < 2 {
            return Err(Error::RecordNotFound)
        }
        let name = self.name.clone();
        let file = self.config.file.clone();
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
    ) -> Result<TimeshiftRecordStreamSource, Error> {
        if self.points.len() < 2 {
            return Err(Error::RecordNotFound)
        }
        let name = self.name.clone();
        let file = self.config.file.clone();
        let record = self.records.get(&record_id).ok_or(Error::ProgramNotFound)?;
        let start = record.start.clone();
        let end = record.end.clone();
        let size = record.get_size(self.config.max_file_size());
        Ok(TimeshiftRecordStreamSource { name, file, start, end, size })
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

    fn handle_chunk(&mut self, point: TimeshiftPoint) {
        self.maintain();
        self.append_point(point);
    }

    fn maintain(&mut self) {
        if self.points.len() < self.config.max_chunks() {
            return;
        }
        self.invalidate_first_chunk();
        self.purge_expired_records();
    }

    fn invalidate_first_chunk(&mut self) {
        assert!(self.points.len() == self.config.max_chunks());
        let point = self.points.remove(0);
        let index = point.pos / (self.config.chunk_size as u64);
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
        let index = point.pos / (self.config.chunk_size as u64);
        assert!(point.pos % (self.config.chunk_size as u64) == 0);
        log::debug!("{}: Chunk#{}: Timestamp: {}", self.name, index, point.timestamp);
        self.points.push(point);
        assert!(self.points.len() <= self.config.max_chunks());
    }

    fn handle_event_start(
        &mut self,
        quad: EventQuad,
        event: EitEvent,
        point: TimeshiftPoint,
    ) {
        let id = self.next_record_id.into();
        self.next_record_id += 1;
        let mut program = EpgProgram::new(quad);
        program.update(&event);
        log::info!("{}: Record#{}: Started: {}: {}", self.name, id, point, program.name());
        self.records.insert(id, TimeshiftRecord {
            id,
            program,
            start: point.clone(),
            end: point.clone(),
        });
    }

    fn handle_event_update(
        &mut self,
        quad: EventQuad,
        event: EitEvent,
        point: TimeshiftPoint,
    ) {
        let mut program = EpgProgram::new(quad);
        program.update(&event);
        self.update_last_record("Updated", program, point);
    }

    fn handle_event_end(
        &mut self,
        quad: EventQuad,
        event: EitEvent,
        point: TimeshiftPoint,
    ) {
        let mut program = EpgProgram::new(quad);
        program.update(&event);
        self.update_last_record("Ended", program, point);
    }

    fn update_last_record(
        &mut self,
        action: &str,
        program: EpgProgram,
        point: TimeshiftPoint,
    ) {
        let last = self.records.values_mut()
            .last()
            .filter(|record| record.program.quad == program.quad);
        if let Some(mut record) = last {
            log::debug!("{}: Record#{}: {}: {}: {}", self.name, record.id, action, point, program.name());
            record.program = program;
            record.end = point;
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
        TimeshiftRecorderModel {
            name: self.name.clone(),
            start_time,
            duration: end_time - start_time,
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
struct TimeshiftChunkMessage {
    id: String,
    chunk: TimeshiftPoint,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TimeshiftEventMessage {
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
    pos: u64,
}

impl fmt::Display for TimeshiftPoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.timestamp, self.pos)
    }
}

#[derive(Clone)]
struct TimeshiftRecord {
    id: TimeshiftRecordId,
    program: EpgProgram,
    start: TimeshiftPoint,
    end: TimeshiftPoint,
}

impl TimeshiftRecord {
    fn get_size(&self, file_size: u64) -> u64 {
        if self.end.pos < self.start.pos {
            file_size - self.start.pos + self.end.pos
        } else {
            self.end.pos - self.start.pos
        }
    }

    fn get_model(&self, config: &TimeshiftConfig) -> TimeshiftRecordModel {
        TimeshiftRecordModel {
            id: self.id,
            program: self.program.clone().into(),
            start_time: self.start.timestamp.clone(),
            duration: self.end.timestamp - self.start.timestamp,
            size: self.get_size(config.max_file_size()),
        }
    }
}

struct TimeshiftStreamSource {
    name: String,
    file: String,
    point: TimeshiftPoint,
}

impl TimeshiftStreamSource {
    // 32 KiB, large enough for 10 ms buffering.
    const CHUNK_SIZE: usize = 4096 * 8;

    async fn create_stream(self) -> Result<TimeshiftStream, Error> {
        log::debug!("{}: Start live streaming from {}", self.name, self.point);
        let mut file = TimeshiftFile::open(&self.file).await?;
        file.set_position(self.point.pos).await?;
        let reader = ChunkStream::new(file, Self::CHUNK_SIZE);
        Ok(MpegTsStream::new(0, reader))  // TODO: id
    }
}

struct TimeshiftRecordStreamSource {
    name: String,
    file: String,
    start: TimeshiftPoint,
    end: TimeshiftPoint,
    size: u64,
}

impl TimeshiftRecordStreamSource {
    // 32 KiB, large enough for 10 ms buffering.
    const CHUNK_SIZE: usize = 4096 * 8;

    async fn create_stream(self) -> Result<TimeshiftRecordStream, Error> {
        log::debug!("{}: Start on-demand streaming from {} to {}",
                    self.name, self.start, self.end);
        let mut file = TimeshiftFile::open(&self.file).await?;
        file.set_position(self.start.pos).await?;
        let file = file.take(self.size);
        let reader = ChunkStream::new(file, Self::CHUNK_SIZE);
        Ok(MpegTsStream::new(0, reader))  // TODO: id
    }
}

pub struct TimeshiftFile {
    state: TimeshiftFileState,
    path: String,
    file: File,
}

enum TimeshiftFileState {
    Read,
    Seek,
    Wait,
}

impl TimeshiftFile {
    async fn open(path: &str) -> Result<Self, Error> {
        Ok(TimeshiftFile {
            state: TimeshiftFileState::Read,
            path: path.to_string(),
            file: File::open(path).await?,
        })
    }

    async fn set_position(&mut self, pos: u64) -> Result<(), Error> {
        let _ = self.file.seek(SeekFrom::Start(pos)).await;
        Ok(())
    }
}

impl AsyncRead for TimeshiftFile {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        loop {
            match self.state {
                TimeshiftFileState::Read => {
                    match Pin::new(&mut self.file).poll_read(cx, buf) {
                        Poll::Ready(Ok(0)) => {
                            self.state = TimeshiftFileState::Seek;
                            log::debug!("{}: EOF reached", self.path);
                        }
                        poll => {
                            return poll;
                        }
                    }
                }
                TimeshiftFileState::Seek => {
                    match Pin::new(&mut self.file).start_seek(cx, SeekFrom::Start(0)) {
                        Poll::Ready(Ok(_)) => {
                            self.state = TimeshiftFileState::Wait;
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
                TimeshiftFileState::Wait => {
                    match Pin::new(&mut self.file).poll_complete(cx) {
                        Poll::Ready(Ok(pos)) => {
                            assert!(pos == 0);
                            self.state = TimeshiftFileState::Read;
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
            channel: create_epg_channel(),
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
                },
            },
            points: vec![
                TimeshiftPoint {
                    timestamp: Jst.ymd(2021, 1, 1).and_hms(0, 1, 0),
                    pos: 0,
                },
            ],
            next_record_id: 0,
            recording: true,
        };
        recorder.purge_expired_records();
        assert!(recorder.records.is_empty());

        let mut recorder = TimeshiftRecorder {
            name: "recorder".to_string(),
            config: create_config(),
            channel: create_epg_channel(),
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
                },
            },
            points: vec![
                TimeshiftPoint {
                    timestamp: Jst.ymd(2021, 1, 1).and_hms(0, 1, 0),
                    pos: 0,
                },
            ],
            next_record_id: 0,
            recording: true,
        };
        recorder.purge_expired_records();
        assert_eq!(recorder.records.len(), 1);
        assert_eq!(recorder.records[0].program.quad, (0, 0, 0, 3).into());
    }

    fn create_config() -> TimeshiftConfig {
        serde_yaml::from_str::<TimeshiftConfig>(r#"
          channel: ch
          sid: 1
          file: /path/to/file
          num-chunks: 5
        "#).unwrap()
    }

    fn create_epg_channel() -> EpgChannel {
        EpgChannel {
            name: "ch".to_string(),
            channel_type: ChannelType::GR,
            channel: "ch".to_string(),
            extra_args: "".to_string(),
            services: vec![],
            excluded_services: vec![],
        }
    }
}
