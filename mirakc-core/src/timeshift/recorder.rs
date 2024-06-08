use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;

use actlet::prelude::*;
use chrono::DateTime;
use chrono::Duration;
use chrono_jst::Jst;
use indexmap::IndexMap;
use serde::Deserialize;
use serde::Serialize;
use tokio::io::AsyncBufReadExt;
use tokio::io::BufReader;

use super::*;
use crate::command_util::*;
use crate::config::*;
use crate::epg::EitEvent;
use crate::epg::EpgChannel;
use crate::epg::EpgProgram;
use crate::epg::EpgService;
use crate::error::Error;
use crate::file_util;
use crate::filter::*;
use crate::models::*;
use crate::tuner::*;

// recorder

pub struct TimeshiftRecorder<T> {
    index: usize,
    name: String,
    config: Arc<Config>,
    tuner_manager: T,

    // data to be stored in the data file.
    service: EpgService,
    // The `end` property of the last record equals to the last item in `points`
    // while recording.
    records: IndexMap<TimeshiftRecordId, TimeshiftRecord>,
    // Each item in `points` corresponds to the start point of a chunk.
    // `points` contains some kind of "garbage" items.  See comments in
    // `save_data()` and `handle_start_recording()` for details.
    points: VecDeque<TimeshiftPoint>,

    service_available: bool,
    recording: bool,
    session: Option<TimeshiftRecorderSession>,
    current_record_id: Option<TimeshiftRecordId>,

    event_emitter: Emitter<TimeshiftEvent>,
}

impl<T> TimeshiftRecorder<T> {
    pub fn new<E>(
        index: usize,
        name: String,
        config: Arc<Config>,
        tuner_manager: T,
        event_emitter: E,
    ) -> Self
    where
        E: Emit<TimeshiftEvent> + Send + Sync + 'static,
    {
        let recorder_config = &config.timeshift.recorders[&name];
        // Dummy data.
        // It will be filled when the data-file is loaded.
        let service = EpgService {
            id: recorder_config.service_id,
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
        let max_chunks = recorder_config.max_chunks();
        TimeshiftRecorder {
            index,
            name,
            config,
            tuner_manager,
            service,
            records: IndexMap::new(),
            points: VecDeque::with_capacity(max_chunks),
            recording: false,
            service_available: false,
            session: None,
            current_record_id: None,
            event_emitter: Emitter::new(event_emitter),
        }
    }

    fn config(&self) -> &TimeshiftRecorderConfig {
        &self.config.timeshift.recorders[&self.name]
    }

    fn load_data(&mut self) {
        match self.do_load_data() {
            Ok(n) => {
                if n == 0 {
                    tracing::debug!(recorder.name = self.name, "No records saved");
                } else {
                    tracing::info!(
                        recorder.name = self.name,
                        records.len = n,
                        "Loaded records successfully",
                    );
                }
            }
            Err(err) => {
                tracing::error!(
                    %err,
                    recorder.name = self.name,
                    "Failed to load records",
                );
                tracing::error!(
                    recorder.name = self.name,
                    "Recover or simply remove <data-file>",
                );
                std::process::exit(libc::EXIT_FAILURE);
                // never reach here
            }
        }
    }

    fn do_load_data(&mut self) -> Result<usize, Error> {
        if !Path::new(&self.config().data_file).exists() {
            return Ok(0);
        }

        let file = std::fs::File::open(&self.config().data_file)?;
        // If the file is empty, serde_json::from_reader() always causes a parse
        // error even though serde_json reads no data actually.
        if file.metadata()?.len() == 0 {
            return Ok(0);
        }

        let data: TimeshiftRecorderData = serde_json::from_reader(file)?;
        let mut invalid = false;
        if self.config().service_id != data.service.id {
            tracing::error!(
                recorder.name = self.name,
                config.service.id =  %self.config().service_id,
                %data.service.id,
                "Not matched",
            );
            invalid = true;
        }
        if self.config().chunk_size != data.chunk_size {
            tracing::error!(
                recorder.name = self.name,
                config.chunk_size = self.config().chunk_size,
                data.chunk_size,
                "Not matched",
            );
            invalid = true;
        }
        if self.config().max_chunks() != data.max_chunks {
            tracing::error!(
                recorder.name = self.name,
                config.max_chunks = self.config().max_chunks(),
                data.max_chunks,
                "Not matched",
            );
            invalid = true;
        }
        if invalid {
            return Err(Error::TimeshiftConfigInconsistent);
        }
        self.service = data.service;
        self.records = data.records;
        self.points = data.points; // Don't remove the last item here.
        Ok(self.records.len())
    }

    fn save_data(&self) {
        let service = &self.service;
        let chunk_size = self.config().chunk_size;
        let max_chunks = self.config().max_chunks();

        let records = &self.records;
        if records.is_empty() {
            tracing::debug!(recorder.name = self.name, "No records to save");
            return;
        }

        // The best way is to remove "garbage" items from `points`, whose
        // timestamp is newer than the timestamp of the last record in
        // `records`.  However, serde has no built-in support for `Iterator`.
        // So, we have to create a new `Vec<TimeshiftPoint>` every time
        // the `points` is saved.
        //
        // We select a better way to save all items in `points` here, and
        // remove "garbage" items before starting a new recording session.
        // See `handle_start_recording()`.
        let points = &self.points;

        let data = TimeshiftRecorderDataForSave {
            service,
            chunk_size,
            max_chunks,
            records,
            points,
        };

        // issue#676
        // ---------
        // In order to keep records as much as possible, we perform the following steps
        //
        //   1. Create <data-file>.new file and write the serialized data to it
        //   2. Rename <data-file>.new to <data-file>
        //
        // If this function fails, inconsistency between <data-file> and <ts-file> happens.
        // mirakc cannot recover this situation by itself and this must be resolved by
        // the user.  For example, the user might have to remove some files in order to
        // make enough space in the filesystem.
        //
        // If mirakc restarts before resolving the inconsistent situation, mirakc will
        // start timeshift recording based on the *old* data file.  As a result, newer
        // records will be lost.  Additionally, TS packets for older records will be
        // lost if a wrap-around occurred in the TS file.
        if !file_util::save_json(data, &self.config().data_file) {
            tracing::error!(
                recorder.name = self.name,
                "Sync between <ts-file> and <data-file> was lost"
            );
            return;
        }

        tracing::debug!(
            recorder.name = self.name,
            records.len = records.len(),
            "Saved records successfully",
        );
    }

    fn get_model(&self) -> TimeshiftRecorderModel {
        let pipeline = self
            .session
            .as_ref()
            .map(|session| session.pipeline.get_model())
            .unwrap_or_default();
        TimeshiftRecorderModel {
            index: self.index,
            name: self.name.clone(),
            service: self.service.clone(),
            start_time: self.start_time(),
            end_time: self.end_time(),
            duration: self.duration(),
            num_records: self.records.len(),
            pipeline,
            recording: self.recording,
            current_record_id: self.current_record_id,
        }
    }

    fn start_time(&self) -> Option<DateTime<Jst>> {
        self.records
            .first()
            .map(|(_, record)| record.start.timestamp)
    }

    fn end_time(&self) -> Option<DateTime<Jst>> {
        self.records
            .last()
            .map(|(_, record)| record.start.timestamp)
    }

    fn duration(&self) -> Duration {
        match (self.start_time(), self.end_time()) {
            (Some(start_time), Some(end_time)) => end_time - start_time,
            _ => Duration::zero(),
        }
    }
}

impl<T> TimeshiftRecorder<T>
where
    T: Clone + Send + Sync + 'static,
    T: Call<StartStreaming>,
    T: TriggerFactory<StopStreaming>,
{
    fn create_live_stream_source(
        &self,
        record_id: Option<TimeshiftRecordId>,
    ) -> Result<TimeshiftLiveStreamSource, Error> {
        if self.points.len() < 2 {
            return Err(Error::RecordNotFound);
        }
        let name = self.name.clone();
        let file = self.config().ts_file.clone();
        let point = if let Some(id) = record_id {
            let record = self.records.get(&id).ok_or(Error::ProgramNotFound)?;
            record.start.clone()
        } else {
            self.points[0].clone()
        };
        Ok(TimeshiftLiveStreamSource { name, file, point })
    }

    fn create_record_stream_source(
        &self,
        record_id: TimeshiftRecordId,
        start_pos: Option<u64>,
    ) -> Result<TimeshiftRecordStreamSource, Error> {
        let record = self.records.get(&record_id).ok_or(Error::ProgramNotFound)?;
        record.create_record_stream_source(self.name.clone(), self.config(), start_pos)
    }

    async fn start_recording(&mut self, ctx: &mut Context<Self>) {
        tracing::debug!(recorder.name = self.name, "Starting recording...");
        match self.do_start_recording(ctx).await {
            Ok(_) => {
                tracing::debug!(recorder.name = self.name, "Recording started successfully");
            }
            Err(err) => {
                tracing::debug!(
                    %err,
                    recorder.name = self.name,
                    "Failed to start recording"
                );
            }
        }
    }

    async fn do_start_recording(&mut self, ctx: &mut Context<Self>) -> Result<(), Error> {
        let config = &self.config.timeshift.recorders[&self.name];
        let channel = &self.service.channel;

        let user = TunerUser {
            info: TunerUserInfo::TimeshiftRecorder(self.name.clone()),
            priority: config.priority.into(),
        };

        let stream = self
            .tuner_manager
            .call(StartStreaming {
                channel: channel.clone(),
                user,
                stream_id: None,
            })
            .await??;

        // stop_trigger must be created here in order to stop streaming when an error occurs.
        let msg = StopStreaming { id: stream.id() };
        let stop_trigger = self.tuner_manager.trigger(msg);

        let data = mustache::MapBuilder::new()
            .insert_str("channel_name", &channel.name)
            .insert("channel_type", &channel.channel_type)?
            .insert_str("channel", &channel.channel)
            .insert("sid", &self.service.sid())?
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
            builder.add_decode_filter(&self.config.filters.decode_filter)?;
        }
        let (mut cmds, _) = builder.build();

        let start_pos = self.points.back().map_or(0, |point| point.pos);
        let data = mustache::MapBuilder::new()
            .insert("sid", &self.service.sid())?
            .insert_str("file", config.ts_file.to_str().unwrap())
            .insert("chunk_size", &config.chunk_size)?
            .insert("num_chunks", &config.num_chunks)?
            .insert("start_pos", &start_pos)?
            .build();
        let template = mustache::compile_str(&self.config.timeshift.command)?;
        cmds.push(template.render_data_to_string(&data)?);

        let mut pipeline = spawn_pipeline(cmds, stream.id(), "timeshift")?;

        let (input, output) = pipeline.take_endpoints();

        ctx.spawn_task(async move {
            let _ = stream.pipe(input).await;
        });

        self.session = Some(TimeshiftRecorderSession {
            pipeline,
            _stop_trigger: stop_trigger,
        });

        let src = PipelineMessageSource::new(self.name.clone(), output, ctx.address().clone());
        ctx.spawn_task(async move {
            src.run().await;
        });

        Ok(())
    }
}

// actor

#[async_trait]
impl<T> Actor for TimeshiftRecorder<T>
where
    T: Clone + Send + Sync + 'static,
    T: Call<StartStreaming>,
    T: TriggerFactory<StopStreaming>,
{
    async fn started(&mut self, _ctx: &mut Context<Self>) {
        tracing::debug!(recorder.name = self.name, "Started");
        self.load_data();
        // A recording will be started in the HealthCheck handler.
    }

    async fn stopped(&mut self, _ctx: &mut Context<Self>) {
        self.session = None;
        tracing::debug!(recorder.name = self.name, "Stopped");
    }
}

#[derive(Serialize)]
struct TimeshiftRecorderDataForSave<'a> {
    service: &'a EpgService,
    chunk_size: usize,
    max_chunks: usize,
    records: &'a IndexMap<TimeshiftRecordId, TimeshiftRecord>,
    points: &'a VecDeque<TimeshiftPoint>,
}

#[async_trait]
impl<T> Handler<ServiceUpdated> for TimeshiftRecorder<T>
where
    T: Clone + Send + Sync + 'static,
    T: Call<StartStreaming>,
    T: TriggerFactory<StopStreaming>,
{
    async fn handle(&mut self, msg: ServiceUpdated, ctx: &mut Context<Self>) {
        if let Some(service) = msg.service {
            self.service = service;
            self.service_available = true;
            tracing::info!(recorder.name = self.name, "Service is now available");
            if self.session.is_none() {
                self.start_recording(ctx).await;
            }
        } else {
            self.service_available = false;
            tracing::info!(recorder.name = self.name, "Service is unavailable");
        }
    }
}

#[async_trait]
impl<T> Handler<QueryTimeshiftRecorder> for TimeshiftRecorder<T>
where
    T: Clone + Send + Sync + 'static,
    T: Call<StartStreaming>,
    T: TriggerFactory<StopStreaming>,
{
    async fn handle(
        &mut self,
        _msg: QueryTimeshiftRecorder,
        _ctx: &mut Context<Self>,
    ) -> <QueryTimeshiftRecorder as Message>::Reply {
        Ok(self.get_model())
    }
}

#[async_trait]
impl<T> Handler<QueryTimeshiftRecords> for TimeshiftRecorder<T>
where
    T: Clone + Send + Sync + 'static,
    T: Call<StartStreaming>,
    T: TriggerFactory<StopStreaming>,
{
    async fn handle(
        &mut self,
        _msg: QueryTimeshiftRecords,
        _ctx: &mut Context<Self>,
    ) -> <QueryTimeshiftRecords as Message>::Reply {
        let records = self
            .records
            .values()
            .map(|record| record.get_model(self.config()))
            .collect();
        Ok(records)
    }
}

#[async_trait]
impl<T> Handler<QueryTimeshiftRecord> for TimeshiftRecorder<T>
where
    T: Clone + Send + Sync + 'static,
    T: Call<StartStreaming>,
    T: TriggerFactory<StopStreaming>,
{
    async fn handle(
        &mut self,
        msg: QueryTimeshiftRecord,
        _ctx: &mut Context<Self>,
    ) -> <QueryTimeshiftRecord as Message>::Reply {
        let result = self
            .records
            .get(&msg.record_id)
            .map(|record| record.get_model(self.config()))
            .ok_or(Error::RecordNotFound);
        result
    }
}

#[async_trait]
impl<T> Handler<CreateTimeshiftLiveStreamSource> for TimeshiftRecorder<T>
where
    T: Clone + Send + Sync + 'static,
    T: Call<StartStreaming>,
    T: TriggerFactory<StopStreaming>,
{
    async fn handle(
        &mut self,
        msg: CreateTimeshiftLiveStreamSource,
        _ctx: &mut Context<Self>,
    ) -> <CreateTimeshiftLiveStreamSource as Message>::Reply {
        self.create_live_stream_source(msg.record_id)
    }
}

#[async_trait]
impl<T> Handler<CreateTimeshiftRecordStreamSource> for TimeshiftRecorder<T>
where
    T: Clone + Send + Sync + 'static,
    T: Call<StartStreaming>,
    T: TriggerFactory<StopStreaming>,
{
    async fn handle(
        &mut self,
        msg: CreateTimeshiftRecordStreamSource,
        _ctx: &mut Context<Self>,
    ) -> <CreateTimeshiftRecordStreamSource as Message>::Reply {
        self.create_record_stream_source(msg.record_id, msg.start_pos)
    }
}

#[derive(Message)]
struct PipelineStarted;

#[async_trait]
impl<T> Handler<PipelineStarted> for TimeshiftRecorder<T>
where
    T: Clone + Send + Sync + 'static,
    T: Call<StartStreaming>,
    T: TriggerFactory<StopStreaming>,
{
    async fn handle(&mut self, _msg: PipelineStarted, _ctx: &mut Context<Self>) {
        tracing::debug!(recorder.name = self.name, msg.name = "PipelineStarted");
        self.recording = true;
        let msg = TimeshiftEvent::Started {
            recorder: self.name.clone(),
        };
        self.event_emitter.emit(msg).await;
    }
}

#[derive(Message)]
struct PipelineStopped;

#[async_trait]
impl<T> Handler<PipelineStopped> for TimeshiftRecorder<T>
where
    T: Clone + Send + Sync + 'static,
    T: Call<StartStreaming>,
    T: TriggerFactory<StopStreaming>,
{
    async fn handle(&mut self, _msg: PipelineStopped, _ctx: &mut Context<Self>) {
        tracing::debug!(recorder.name = self.name, msg.name = "PipelineStopped");
        self.recording = false;
        self.session = None;
        let msg = TimeshiftEvent::Stopped {
            recorder: self.name.clone(),
        };
        self.event_emitter.emit(msg).await;
    }
}

#[async_trait]
impl<T> Handler<HealthCheck> for TimeshiftRecorder<T>
where
    T: Clone + Send + Sync + 'static,
    T: Call<StartStreaming>,
    T: TriggerFactory<StopStreaming>,
{
    async fn handle(
        &mut self,
        _msg: HealthCheck,
        ctx: &mut Context<Self>,
    ) -> <HealthCheck as Message>::Reply {
        tracing::debug!(recorder.name = self.name, msg.name = "HealthCheck");

        if self.session.is_some() {
            // Now recording, nothing to do.
            return;
        }

        if !self.service_available {
            // The service is unavailable.
            // Will start recording when the service becomes available.
            return;
        }

        self.start_recording(ctx).await;
    }
}

// pipeline message

#[derive(Deserialize, Message)]
#[serde(rename_all = "kebab-case")]
#[serde(tag = "type", content = "data")]
enum PipelineMessage {
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
    original_network_id: Nid,
    #[allow(dead_code)]
    transport_stream_id: Tsid,
    service_id: Sid,
    event: EitEvent,
    record: TimeshiftPoint,
}

#[async_trait]
impl<T> Handler<PipelineMessage> for TimeshiftRecorder<T>
where
    T: Clone + Send + Sync + 'static,
    T: Call<StartStreaming>,
    T: TriggerFactory<StopStreaming>,
{
    async fn handle(&mut self, msg: PipelineMessage, _ctx: &mut Context<Self>) {
        match msg {
            PipelineMessage::Start => {
                self.handle_start_recording().await;
            }
            PipelineMessage::Stop(msg) => {
                self.handle_stop_recording(msg.reset).await;
            }
            PipelineMessage::Chunk(msg) => {
                self.handle_chunk(msg.chunk).await;
            }
            PipelineMessage::EventStart(msg) => {
                let program_id =
                    ProgramId::new(msg.original_network_id, msg.service_id, msg.event.event_id);
                self.handle_event_start(program_id, msg.event, msg.record)
                    .await;
            }
            PipelineMessage::EventUpdate(msg) => {
                let program_id =
                    ProgramId::new(msg.original_network_id, msg.service_id, msg.event.event_id);
                self.handle_event_update(program_id, msg.event, msg.record)
                    .await;
            }
            PipelineMessage::EventEnd(msg) => {
                let program_id =
                    ProgramId::new(msg.original_network_id, msg.service_id, msg.event.event_id);
                self.handle_event_end(program_id, msg.event, msg.record)
                    .await;
            }
        }
    }
}

impl<T> TimeshiftRecorder<T> {
    async fn handle_start_recording(&mut self) {
        tracing::info!(recorder.name = self.name, "Recording started");

        // As described in `save_data()`, we save `points` including "garbage"
        // items.  Here we remove them before starting a new recording session.
        // And we additionally check consistency between `records` and `points`
        // for safety.
        self.remove_garbage_points();
        self.check_consistency();
    }

    fn remove_garbage_points(&mut self) {
        if let Some(last_record) = self.records.values().last() {
            while let Some(last_point) = self.points.back() {
                // Don't use `pos`.  It's a position in a "ring" buffer and
                // doesn't increase monotonically.
                if last_record.end.timestamp > last_point.timestamp {
                    break;
                }
                let point = self.points.pop_back().unwrap();
                tracing::debug!(
                    recorder.name = self.name,
                    %point,
                    "Removed garbage point \
                     before starting a new recording session"
                );
            }
        }
    }

    fn check_consistency(&self) {
        if let Some(last_record) = self.records.values().last() {
            let broken = if let Some(last_point) = self.points.back() {
                let pos = last_point.pos + self.config().chunk_size as u64;
                last_record.end.pos != pos % self.config().max_file_size()
            } else {
                true
            };
            if broken {
                tracing::error!(
                    recorder.name = self.name,
                    "INCONSISTENT: data-file may be broken, \
                     rebuild timeshift files or remove the data file"
                );
                std::process::exit(libc::EXIT_FAILURE);
            }
        }
    }

    async fn handle_stop_recording(&mut self, reset: bool) {
        tracing::info!(recorder.name = self.name, "Recording stopped");
        if reset {
            tracing::warn!(recorder.name = self.name, "Reset data");
            // TODO
        }

        self.current_record_id = None;
    }

    async fn handle_chunk(&mut self, point: TimeshiftPoint) {
        self.maintain();
        self.append_point(&point);
        self.save_data();
        let msg = TimeshiftEvent::Timeline {
            recorder: self.name.clone(),
            start_time: self.start_time(),
            end_time: self.end_time(),
            duration: self.duration(),
        };
        self.event_emitter.emit(msg).await;
    }

    async fn handle_event_start(
        &mut self,
        program_id: ProgramId,
        event: EitEvent,
        point: TimeshiftPoint,
    ) {
        // Multiple records for the same TV program may be created when the
        // timeshift recording restarts.  Therefore, we use the recording start
        // time as the identifier of the record, instead of the start time in
        // the EPG data.
        let record_id = point.timestamp.timestamp().into();

        let mut program = EpgProgram::new(program_id);
        program.update(&event);
        let record = TimeshiftRecord::new(record_id, program, point);
        tracing::info!(
            recorder.name = self.name,
            %record.id,
            %record.program.id,
            %record.start,
            "Record started"
        );
        self.records.insert(record_id, record);
        self.current_record_id = Some(record_id);

        let msg = TimeshiftEvent::RecordStarted {
            recorder: self.name.clone(),
            record_id,
        };
        self.event_emitter.emit(msg).await;
    }

    async fn handle_event_update(
        &mut self,
        program_id: ProgramId,
        event: EitEvent,
        point: TimeshiftPoint,
    ) {
        let record_id = point.timestamp.timestamp().into();
        let mut program = EpgProgram::new(program_id);
        program.update(&event);
        self.update_last_record(program, point, false);

        let msg = TimeshiftEvent::RecordUpdated {
            recorder: self.name.clone(),
            record_id,
        };
        self.event_emitter.emit(msg).await;
    }

    async fn handle_event_end(
        &mut self,
        program_id: ProgramId,
        event: EitEvent,
        point: TimeshiftPoint,
    ) {
        let mut program = EpgProgram::new(program_id);
        program.update(&event);
        self.update_last_record(program, point, true);
        self.current_record_id = None;
    }

    fn maintain(&mut self) {
        if self.points.len() < self.config().max_chunks() {
            return;
        }
        self.invalidate_first_chunk();
        self.purge_expired_records();
        self.crop_record_to_be_overwritten();
    }

    fn invalidate_first_chunk(&mut self) {
        assert!(self.points.len() == self.config().max_chunks());
        let point = self.points.pop_front().unwrap();
        let index = point.pos / (self.config().chunk_size as u64);
        tracing::debug!(recorder.name = self.name, chunk = index, %point, "Chunk invalidated");
    }

    // Purge records which ended recording before the first timestamp.
    //
    // A record which started recording before the timestamp and ended recording after the
    // timestamp it NOT purged.  Because it has data in available chunks and can be provided
    // via Web endpoints for streaming.
    fn purge_expired_records(&mut self) {
        assert!(!self.points.is_empty());
        let timestamp = self.points[0].timestamp;
        let n = self
            .records
            .values()
            .position(|record| record.end.timestamp > timestamp)
            .unwrap_or(self.records.len());
        for (_, record) in self.records.drain(0..n) {
            // remove first n records
            tracing::info!(recorder.name = self.name, %record.id, %record.program.id, "Record purged");
        }
    }

    // Crop the beginning of a record to be overwritten.
    //
    // REMARKS
    // -------
    // The mtime and ctime of a file that corresponds to the cropped record exposed onto a
    // timeshift-fs are not changed.  That may cause issues in other applications.
    fn crop_record_to_be_overwritten(&mut self) {
        assert!(!self.points.is_empty());
        let start = self.points[0].clone();
        // Checking the first record is enough because old records have already been purged in
        // purge_expired_records().
        if let Some((_, record)) = self.records.first_mut() {
            if record.start.timestamp < start.timestamp {
                record.start = start;
                tracing::debug!(recorder.name = self.name, %record.id, %record.program.id, %record.start, "Record cropped");
            }
        }
    }

    fn append_point(&mut self, point: &TimeshiftPoint) {
        let index = point.pos / (self.config().chunk_size as u64);
        assert!(point.pos % (self.config().chunk_size as u64) == 0);
        tracing::debug!(recorder.name = self.name, chunk = index, %point, "Chunk started");
        self.points.push_back(point.clone());
        assert!(self.points.len() <= self.config().max_chunks());
    }

    fn update_last_record(&mut self, program: EpgProgram, point: TimeshiftPoint, end: bool) {
        match self.records.values_mut().last() {
            Some(record) => {
                record.update(program, point, end);
                if end {
                    tracing::info!(recorder.name = self.name, %record.id, %record.program.id, %record.end, "Record ended");
                } else {
                    tracing::debug!(recorder.name = self.name, %record.id, %record.program.id, %record.end, "Record updated");
                }
            }
            None => {
                tracing::warn!(recorder.name = self.name, "No record to update");
            }
        }
    }
}

// models

struct TimeshiftRecorderSession {
    pipeline: CommandPipeline<TunerSubscriptionId>,
    _stop_trigger: Trigger<StopStreaming>,
}

// pipeline message source
struct PipelineMessageSource<T> {
    recorder_name: String,
    reader: CommandPipelineOutput<TunerSubscriptionId>,
    addr: T,
}

impl<T> PipelineMessageSource<T>
where
    T: Emit<PipelineMessage>,
    T: Emit<PipelineStarted>,
    T: TriggerFactory<PipelineStopped>,
{
    fn new(
        recorder_name: String,
        reader: CommandPipelineOutput<TunerSubscriptionId>,
        addr: T,
    ) -> Self {
        PipelineMessageSource {
            recorder_name,
            reader,
            addr,
        }
    }

    async fn run(self) {
        let PipelineMessageSource {
            recorder_name,
            reader,
            addr,
        } = self;
        addr.emit(PipelineStarted).await;
        let _trigger = addr.trigger(PipelineStopped);
        let reader = BufReader::new(reader);
        let mut lines = reader.lines();
        loop {
            match lines.next_line().await {
                Ok(Some(json)) => {
                    let msg = match serde_json::from_str::<PipelineMessage>(&json) {
                        Ok(msg) => msg,
                        Err(err) => {
                            tracing::debug!(
                                %err,
                                recorder.name = recorder_name,
                                json,
                                "Failed parsing a JSON message, jsut ignore it"
                            );
                            continue;
                        }
                    };
                    addr.emit(msg).await;
                }
                Ok(None) => {
                    // EOF
                    return;
                }
                Err(err) => {
                    tracing::error!(
                        %err,
                        recorder.name = recorder_name,
                        "Failed reading output from the command pipeline"
                    );
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::broadcaster::BroadcasterStream;
    use crate::mpeg_ts_stream::MpegTsStream;
    use crate::tuner::stub::TunerManagerStub;
    use assert_matches::assert_matches;
    use tempfile::TempDir;
    use test_log::test;
    use tokio::sync::Notify;

    macro_rules! point {
        ($timestamp:literal, $pos:expr) => {
            TimeshiftPoint {
                timestamp: jst!($timestamp),
                pos: $pos,
            }
        };
    }

    macro_rules! record {
        ($id:expr, $program:expr, $start:expr, $end:expr) => {
            TimeshiftRecord {
                id: $id.into(),
                program: $program,
                start: $start,
                end: $end,
                recording: false,
            }
        };
    }

    macro_rules! recorder {
        ($config:expr) => {
            TimeshiftRecorder::new(
                0,
                "test".to_string(),
                $config,
                TunerManagerStub::default(),
                EventEmitterStub,
            )
        };
        ($config:expr, $tuner_manager:expr, $event_emitter:expr) => {
            TimeshiftRecorder::new(
                0,
                "test".to_string(),
                $config,
                $tuner_manager,
                $event_emitter,
            )
        };
    }
    #[test(tokio::test)]
    async fn test_timeshift_record_purge_expired_records() {
        let temp_dir = TempDir::new().unwrap();
        let config = create_config(temp_dir.path());

        let mut recorder = recorder!(config.clone());
        recorder.records = indexmap::indexmap! {
            1u32.into() => record! {
                1u32,
                program!((0, 1, 1)),
                point!("2021-01-01T00:00:00+09:00", 0),
                point!("2021-01-01T00:00:00+09:00", 0)
            },
        };
        recorder.points = vecdeque![point!("2021-01-01T00:01:00+09:00", 0)];
        recorder.purge_expired_records();
        assert!(recorder.records.is_empty());

        let mut recorder = recorder!(config.clone());
        recorder.records = indexmap::indexmap! {
            1u32.into() => record! {
                1u32,
                program!((0, 1, 1)),
                point!("2021-01-01T00:00:00+09:00", 0),
                point!("2021-01-01T00:00:00+09:00", 0)
            },
            2u32.into() => record! {
                2u32,
                program!((0, 1, 2)),
                point!("2021-01-01T00:00:00+09:00", 0),
                point!("2021-01-01T00:01:00+09:00", 0)
            },
            3u32.into() => record! {
                3u32,
                program!((0, 1, 3)),
                point!("2021-01-01T00:00:00+09:00", 0),
                point!("2021-01-01T00:02:00+09:00", 0)
            },
        };
        recorder.points = vecdeque![point!("2021-01-01T00:01:00+09:00", 0)];
        recorder.purge_expired_records();
        assert_eq!(recorder.records.len(), 1);
        assert_eq!(recorder.records[0].program.id, (0, 1, 3).into());
    }

    #[test(tokio::test)]
    async fn test_timeshift_recorder_broken_pipeline() {
        let system = System::new();

        let temp_dir = TempDir::new().unwrap();
        let config = create_config(temp_dir.path());

        let broken = Arc::new(Notify::new());
        let tuner_manager = BrokenTunerManagerStub(broken.clone());

        let (observer, started, stopped) = Observer::new();

        let recorder = system
            .spawn_actor(recorder!(config, tuner_manager, observer))
            .await;

        recorder
            .emit(ServiceUpdated {
                service: Some(create_epg_service()),
            })
            .await;
        started.notified().await;
        let msg = QueryTimeshiftRecorder {
            recorder: TimeshiftRecorderQuery::ByIndex(0), // dummy
        };
        let result = recorder.call(msg).await;
        assert_matches!(result, Ok(Ok(model)) => {
            assert!(model.recording);
        });

        // emulate a broken pipeline
        broken.notify_one();
        stopped.notified().await;
        let msg = QueryTimeshiftRecorder {
            recorder: TimeshiftRecorderQuery::ByIndex(0), // dummy
        };
        let result = recorder.call(msg).await;
        assert_matches!(result, Ok(Ok(model)) => {
            assert!(!model.recording);
        });

        // reactivate
        recorder.call(HealthCheck).await.unwrap();
        started.notified().await;
        let msg = QueryTimeshiftRecorder {
            recorder: TimeshiftRecorderQuery::ByIndex(0), // dummy
        };
        let result = recorder.call(msg).await;
        assert_matches!(result, Ok(Ok(model)) => {
            assert!(model.recording);
        });

        system.stop();
    }

    #[test(tokio::test)]
    async fn test_timeshift_recorder_stop() {
        let system = System::new();

        let temp_dir = TempDir::new().unwrap();
        let config = create_config(temp_dir.path());

        let notify = Arc::new(Notify::new());
        let tuner_manager = BrokenTunerManagerStub(notify.clone());

        let (observer, started, _stopped) = Observer::new();

        let recorder = system
            .spawn_actor(recorder!(config, tuner_manager, observer))
            .await;

        recorder
            .emit(ServiceUpdated {
                service: Some(create_epg_service()),
            })
            .await;
        started.notified().await;
        let msg = QueryTimeshiftRecorder {
            recorder: TimeshiftRecorderQuery::ByIndex(0), // dummy
        };
        let result = recorder.call(msg).await;
        assert_matches!(result, Ok(Ok(model)) => {
            assert!(model.recording);
        });

        recorder.emit(actlet::Stop).await;
        recorder.wait().await;

        system.stop();
    }

    fn create_config<P: AsRef<Path>>(dir: P) -> Arc<Config> {
        let ts_file = dir.as_ref().join("ts-file.m2ts");
        let data_file = dir.as_ref().join("data-file.json");
        Arc::new(
            serde_yml::from_str::<Config>(&format!(
                r#"
                timeshift:
                  command: true
                  recorders:
                    test:
                      service-id: 1
                      ts-file: {}
                      data-file: {}
                      num-chunks: 100
                      uses:
                        tuner: tuner
                        channel-type: GR
                        channel: ch
                "#,
                ts_file.display(),
                data_file.display()
            ))
            .unwrap(),
        )
    }

    fn create_epg_service() -> EpgService {
        service!(1, "Service", channel_gr!("ch", "ch"))
    }

    #[derive(Clone)]
    struct BrokenTunerManagerStub(Arc<Notify>);

    #[async_trait]
    impl Call<StartStreaming> for BrokenTunerManagerStub {
        async fn call(
            &self,
            _msg: StartStreaming,
        ) -> actlet::Result<<StartStreaming as Message>::Reply> {
            let (tx, stream) = BroadcasterStream::new_for_test();
            let notify = self.0.clone();
            tokio::spawn(async move {
                notify.notified().await;
                drop(tx);
            });
            Ok(Ok(MpegTsStream::new(
                TunerSubscriptionId::default(),
                stream,
            )))
        }
    }

    stub_impl_fire! {BrokenTunerManagerStub, StopStreaming}

    #[derive(Clone)]
    struct EventEmitterStub;
    stub_impl_emit! {EventEmitterStub, TimeshiftEvent}

    struct Observer {
        started: Arc<Notify>,
        stopped: Arc<Notify>,
    }

    impl Observer {
        fn new() -> (Observer, Arc<Notify>, Arc<Notify>) {
            let started = Arc::new(Notify::new());
            let stopped = Arc::new(Notify::new());
            let observer = Observer {
                started: started.clone(),
                stopped: stopped.clone(),
            };
            (observer, started, stopped)
        }
    }

    #[async_trait]
    impl Emit<TimeshiftEvent> for Observer {
        async fn emit(&self, msg: TimeshiftEvent) {
            use TimeshiftEvent::*;
            match msg {
                Started { .. } => self.started.notify_one(),
                Stopped { .. } => self.stopped.notify_one(),
                _ => (),
            }
        }
    }
}
