use std::fmt;
use std::fmt::Write as _;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::pin::Pin;
use std::sync::Arc;

use actix::prelude::*;
use actix_files;
use actix_service;
use actix_web::{self, FromRequest};
use actix_web::web::{Bytes, BytesMut};
use chrono::{DateTime, Duration};
use futures;
use futures::stream::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_qs;
use tokio::sync::mpsc;

use crate::airtime_tracker;
use crate::chunk_stream::ChunkStream;
use crate::command_util::*;
use crate::config::{Config, ServerAddr};
use crate::datetime_ext::{serde_jst, serde_duration_in_millis, Jst};
use crate::error::Error;
use crate::epg::*;
use crate::filter::*;
use crate::models::*;
use crate::mpeg_ts_stream::*;
use crate::string_table::*;
use crate::timeshift::*;
use crate::tuner::*;

#[cfg(not(test))]
type TunerManagerActor = TunerManager;
#[cfg(test)]
type TunerManagerActor = actix::actors::mocker::Mocker<TunerManager>;

#[cfg(not(test))]
type EpgActor = Epg;
#[cfg(test)]
type EpgActor = actix::actors::mocker::Mocker<Epg>;

#[cfg(not(test))]
type TimeshiftManagerActor = TimeshiftManager;
#[cfg(test)]
type TimeshiftManagerActor = actix::actors::mocker::Mocker<TimeshiftManager>;

pub async fn serve(
    config: Arc<Config>,
    string_table: Arc<StringTable>,
    tuner_manager: Addr<TunerManager>,
    epg: Addr<Epg>,
    timeshift_manager: Addr<TimeshiftManager>,
) -> Result<(), Error> {
    let server_config = config.server.clone();
    let mut server = actix_web::HttpServer::new(
        move || {
            let app = actix_web::App::new()
                .data(config.clone())
                .data(string_table.clone())
                .data(tuner_manager.clone())
                .data(epg.clone())
                .data(timeshift_manager.clone())
                .wrap(actix_web::middleware::Logger::default())
                .wrap(actix_web::middleware::DefaultHeaders::new()
                      .header("Server", server_name()))
                .wrap(AccessControl)
                .service(create_api_service());
            config.server.mounts.iter().fold(app, |app, (mount_point, mount)| {
                let sv = actix_files::Files::new(&mount_point, &mount.path)
                    .disable_content_disposition();
                let sv = if let Some(ref index_file) = mount.index {
                    sv.index_file(index_file)
                } else {
                    sv
                };
                let sv = if mount.listing {
                    sv.show_files_listing()
                } else {
                    sv
                };
                app.service(sv)
            })
        });
    for addr in server_config.addrs.iter() {
        server = match addr {
            ServerAddr::Http(addr) => server.bind(addr.as_str())?,
            ServerAddr::Unix(path) => server.bind_uds(path.as_str())?,
        };
    }
    server
        .keep_alive(0)  // disable keep-alive
        .workers(server_config.workers)
        .run()
        .await?;
    Ok(())
}

fn server_name() -> String {
    format!("{}/{}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
}

// rest api

type ApiResult = Result<actix_web::HttpResponse, Error>;

#[derive(Serialize)]
struct ErrorBody {
    pub code: u16,
    pub reason: Option<&'static str>,
    pub errors: Vec<u8>,
}

impl actix_web::ResponseError for Error {
    fn error_response(&self) -> actix_web::HttpResponse {
        match *self {
            Error::StreamingTimedOut =>
                actix_web::HttpResponse::RequestTimeout().json(ErrorBody {
                    code: actix_web::http::StatusCode::REQUEST_TIMEOUT.as_u16(),
                    reason: None,
                    errors: Vec::new(),
                }),
            Error::TunerUnavailable =>
                actix_web::HttpResponse::NotFound().json(ErrorBody {
                    code: actix_web::http::StatusCode::NOT_FOUND.as_u16(),
                    reason: None,
                    errors: Vec::new(),
                }),
            Error::ChannelNotFound =>
                actix_web::HttpResponse::NotFound().json(ErrorBody {
                    code: actix_web::http::StatusCode::NOT_FOUND.as_u16(),
                    reason: None,
                    errors: Vec::new(),
                }),
            Error::ServiceNotFound =>
                actix_web::HttpResponse::NotFound().json(ErrorBody {
                    code: actix_web::http::StatusCode::NOT_FOUND.as_u16(),
                    reason: None,
                    errors: Vec::new(),
                }),
            Error::ProgramNotFound =>
                actix_web::HttpResponse::NotFound().json(ErrorBody {
                    code: actix_web::http::StatusCode::NOT_FOUND.as_u16(),
                    reason: None,
                    errors: Vec::new(),
                }),
            Error::RecordNotFound =>
                actix_web::HttpResponse::NotFound().json(ErrorBody {
                    code: actix_web::http::StatusCode::NOT_FOUND.as_u16(),
                    reason: None,
                    errors: Vec::new(),
                }),
            Error::OutOfRange =>
                actix_web::HttpResponse::RangeNotSatisfiable().json(ErrorBody {
                    code: actix_web::http::StatusCode::RANGE_NOT_SATISFIABLE.as_u16(),
                    reason: None,
                    errors: Vec::new(),
                }),
            Error::NoContent =>
                actix_web::HttpResponse::NoContent().json(ErrorBody {
                    code: actix_web::http::StatusCode::NO_CONTENT.as_u16(),
                    reason: None,
                    errors: Vec::new(),
                }),
            Error::AccessDenied =>
                actix_web::HttpResponse::Forbidden().json(ErrorBody {
                    code: actix_web::http::StatusCode::FORBIDDEN.as_u16(),
                    reason: None,
                    errors: Vec::new(),
                }),
            _ =>
                actix_web::HttpResponse::InternalServerError().json(ErrorBody {
                    code: actix_web::http::StatusCode::INTERNAL_SERVER_ERROR
                        .as_u16(),
                    reason: None,
                    errors: Vec::new(),
                }),
        }
    }
}

fn create_api_service() -> impl actix_web::dev::HttpServiceFactory {
    actix_web::web::scope("/api")
        .service(get_version)
        .service(get_status)
        .service(get_channels)
        .service(get_services)
        .service(get_service)
        .service(get_programs)
        .service(get_program)
        .service(get_tuners)
        .service(get_channel_stream)
        .service(get_channel_service_stream)
        .service(get_service_stream)
        .service(get_program_stream)
        .service(get_timeshift_recorders)
        .service(get_timeshift_recorder)
        .service(get_timeshift_records)
        .service(get_timeshift_record)
        .service(get_timeshift_stream)
        .service(get_timeshift_record_stream)
        .service(get_iptv_playlist)
        .service(get_iptv_epg)
        .service(get_docs)
}

#[actix_web::get("/version")]
async fn get_version() -> ApiResult {
    Ok(actix_web::HttpResponse::Ok().json(env!("CARGO_PKG_VERSION")))
}

#[actix_web::get("/status")]
async fn get_status() -> ApiResult {
    Ok(actix_web::HttpResponse::Ok()
       .content_type("application/json").body("{}"))
}

#[actix_web::get("/channels")]
async fn get_channels(
    epg: actix_web::web::Data<Addr<EpgActor>>,
) -> ApiResult {
    epg.send(QueryChannelsMessage).await?
        .map(|channels| actix_web::HttpResponse::Ok().json(channels))
}

#[actix_web::get("/services")]
async fn get_services(
    epg: actix_web::web::Data<Addr<EpgActor>>,
) -> ApiResult {
    epg.send(QueryServicesMessage).await?
        .map(|services| services.into_iter()
             .map(MirakurunService::from)
             .collect::<Vec<MirakurunService>>())
        .map(|services| actix_web::HttpResponse::Ok().json(services))
}

#[actix_web::get("/services/{id}")]
async fn get_service(
    epg: actix_web::web::Data<Addr<EpgActor>>,
    path: actix_web::web::Path<ServicePath>,
) -> ApiResult {
    epg.send(QueryServiceMessage::ByNidSid {
        nid: path.id.nid(),
        sid: path.id.sid(),
    }).await?
        .map(MirakurunService::from)
        .map(|service| actix_web::HttpResponse::Ok().json(service))
}

#[actix_web::get("/programs")]
async fn get_programs(
    epg: actix_web::web::Data<Addr<EpgActor>>,
) -> ApiResult {
    epg.send(QueryProgramsMessage).await?
        .map(|programs| programs.into_iter()
             .map(MirakurunProgram::from)
             .collect::<Vec<MirakurunProgram>>())
        .map(|programs| actix_web::HttpResponse::Ok().json(programs))
}

#[actix_web::get("/programs/{id}")]
async fn get_program(
    epg: actix_web::web::Data<Addr<EpgActor>>,
    path: actix_web::web::Path<ProgramPath>,
) -> ApiResult {
    epg.send(QueryProgramMessage::ByNidSidEid {
        nid: path.id.nid(),
        sid: path.id.sid(),
        eid: path.id.eid(),
    }).await?
        .map(MirakurunProgram::from)
        .map(|program| actix_web::HttpResponse::Ok().json(program))
}

#[actix_web::get("/tuners")]
async fn get_tuners(
    tuner_manager: actix_web::web::Data<Addr<TunerManagerActor>>
) -> ApiResult {
    tuner_manager.send(QueryTunersMessage).await?
        .map(|tuners| actix_web::HttpResponse::Ok().json(tuners))
}

#[actix_web::get("/channels/{channel_type}/{channel}/stream")]
async fn get_channel_stream(
    config: actix_web::web::Data<Arc<Config>>,
    tuner_manager: actix_web::web::Data<Addr<TunerManagerActor>>,
    epg: actix_web::web::Data<Addr<EpgActor>>,
    path: actix_web::web::Path<ChannelPath>,
    user: TunerUser,
    filter_setting: FilterSetting,
) -> ApiResult {
    let channel = epg.send(QueryChannelMessage {
        channel_type: path.channel_type,
        channel: path.channel.clone(),
    }).await??;

    let stream = tuner_manager.send(StartStreamingMessage {
        channel: channel.clone(),
        user: user.clone(),
    }).await??;

    // stop_trigger must be created here in order to stop streaming when an error occurs.
    let stop_trigger = TunerStreamStopTrigger::new(
        stream.id(), tuner_manager.get_ref().clone().recipient());

    let data = mustache::MapBuilder::new()
        .insert_str("channel_name", &channel.name)
        .insert("channel_type", &channel.channel_type)?
        .insert_str("channel", &channel.channel)
        .build();

    let mut builder = FilterPipelineBuilder::new(data);
    builder.add_pre_filters(
        &config.pre_filters, &filter_setting.pre_filters)?;
    if !stream.is_decoded() && filter_setting.decode {
        builder.add_decode_filter(&config.filters.decode_filter)?;
    }
    builder.add_post_filters(
        &config.post_filters, &filter_setting.post_filters)?;
    let (filters, content_type) = builder.build();

    streaming(&config, user, stream, filters, content_type, stop_trigger).await
}

#[actix_web::get("/channels/{channel_type}/{channel}/services/{sid}/stream")]
async fn get_channel_service_stream(
    config: actix_web::web::Data<Arc<Config>>,
    tuner_manager: actix_web::web::Data<Addr<TunerManagerActor>>,
    epg: actix_web::web::Data<Addr<EpgActor>>,
    path: actix_web::web::Path<ChannelServicePath>,
    user: TunerUser,
    filter_setting: FilterSetting,
) -> ApiResult {
    let channel = epg.send(QueryChannelMessage {
        channel_type: path.channel_type,
        channel: path.channel.clone(),
    }).await??;

    do_get_service_stream(
        config, tuner_manager, channel, path.sid, user, filter_setting).await
}

#[actix_web::get("/services/{id}/stream")]
async fn get_service_stream(
    config: actix_web::web::Data<Arc<Config>>,
    tuner_manager: actix_web::web::Data<Addr<TunerManagerActor>>,
    epg: actix_web::web::Data<Addr<EpgActor>>,
    path: actix_web::web::Path<ServicePath>,
    user: TunerUser,
    filter_setting: FilterSetting,
) -> ApiResult {
    let service = epg.send(QueryServiceMessage::ByNidSid {
        nid: path.id.nid(),
        sid: path.id.sid(),
    }).await??;

    do_get_service_stream(
        config, tuner_manager, service.channel, service.sid, user,
        filter_setting).await
}

#[actix_web::get("/programs/{id}/stream")]
async fn get_program_stream(
    config: actix_web::web::Data<Arc<Config>>,
    tuner_manager: actix_web::web::Data<Addr<TunerManagerActor>>,
    epg: actix_web::web::Data<Addr<EpgActor>>,
    path: actix_web::web::Path<ProgramPath>,
    user: TunerUser,
    filter_setting: FilterSetting,
) -> ApiResult {
    let program = epg.send(QueryProgramMessage::ByNidSidEid {
        nid: path.id.nid(),
        sid: path.id.sid(),
        eid: path.id.eid(),
    }).await??;

    let service = epg.send(QueryServiceMessage::ByNidSid {
        nid: path.id.nid(),
        sid: path.id.sid(),
    }).await??;

    let clock = epg.send(QueryClockMessage {
        triple: service.triple(),
    }).await??;

    let stream = tuner_manager.send(StartStreamingMessage {
        channel: service.channel.clone(),
        user: user.clone(),
    }).await??;

    // stream_stop_trigger must be created here in order to stop streaming when an error occurs.
    let stream_stop_trigger = TunerStreamStopTrigger::new(
        stream.id(), tuner_manager.get_ref().clone().recipient());

    let video_tags: Vec<u8> = program.video
        .iter()
        .map(|video| video.component_tag)
        .collect();

    let audio_tags: Vec<u8> = program.audios
        .values()
        .map(|audio| audio.component_tag)
        .collect();

    let data = mustache::MapBuilder::new()
        .insert_str("channel_name", &service.channel.name)
        .insert("channel_type", &service.channel.channel_type)?
        .insert_str("channel", &service.channel.channel)
        .insert("sid", &program.quad.sid().value())?
        .insert("eid", &program.quad.eid().value())?
        .insert("clock_pid", &clock.pid)?
        .insert("clock_pcr", &clock.pcr)?
        .insert("clock_time", &clock.time)?
        .insert("video_tags", &video_tags)?
        .insert("audio_tags", &audio_tags)?
        .build();

    let mut builder = FilterPipelineBuilder::new(data);
    builder.add_pre_filters(&config.pre_filters, &filter_setting.pre_filters)?;
    if !stream.is_decoded() && filter_setting.decode {
        builder.add_decode_filter(&config.filters.decode_filter)?;
    }
    builder.add_program_filter(&config.filters.program_filter)?;
    builder.add_post_filters(
        &config.post_filters, &filter_setting.post_filters)?;
    let (filters, content_type) = builder.build();

    let tracker_stop_trigger = airtime_tracker::track_airtime(
        &config.recorder.track_airtime_command, &service.channel, &program,
        stream.id(), tuner_manager.get_ref().clone(), epg.get_ref().clone()
    ).await?;

    let stop_triggers = vec![stream_stop_trigger, tracker_stop_trigger];

    let result =
        streaming(&config, user, stream, filters, content_type, stop_triggers).await;

    match result {
        Err(Error::ProgramNotFound) =>
            log::warn!("No stream for the program#{}, maybe canceled", path.id),
        _ => (),
    }

    result
}

#[actix_web::get("/timeshift")]
async fn get_timeshift_recorders(
    timeshift_manager: actix_web::web::Data<Addr<TimeshiftManagerActor>>,
) -> ApiResult {
    timeshift_manager.send(QueryTimeshiftRecordersMessage).await?
        .map(|recorders| recorders.into_iter()
             .map(WebTimeshiftRecorder::from)
             .collect::<Vec<WebTimeshiftRecorder>>())
        .map(|recorders| actix_web::HttpResponse::Ok().json(recorders))
}

#[actix_web::get("/timeshift/{recorder}")]
async fn get_timeshift_recorder(
    timeshift_manager: actix_web::web::Data<Addr<TimeshiftManagerActor>>,
    path: actix_web::web::Path<TimeshiftRecorderPath>,
) -> ApiResult {
    timeshift_manager.send(QueryTimeshiftRecorderMessage {
        recorder_name: path.recorder.clone(),
    }).await?
        .map(WebTimeshiftRecorder::from)
        .map(|recorder| actix_web::HttpResponse::Ok().json(recorder))
}

#[actix_web::get("/timeshift/{recorder}/records")]
async fn get_timeshift_records(
    timeshift_manager: actix_web::web::Data<Addr<TimeshiftManagerActor>>,
    path: actix_web::web::Path<TimeshiftRecorderPath>,
) -> ApiResult {
    timeshift_manager.send(QueryTimeshiftRecordsMessage {
        recorder_name: path.recorder.clone(),
    }).await?
        .map(|records| records.into_iter()
             .map(WebTimeshiftRecord::from)
             .collect::<Vec<WebTimeshiftRecord>>())
        .map(|records| actix_web::HttpResponse::Ok().json(records))
}

#[actix_web::get("/timeshift/{recorder}/records/{record}")]
async fn get_timeshift_record(
    timeshift_manager: actix_web::web::Data<Addr<TimeshiftManagerActor>>,
    path: actix_web::web::Path<TimeshiftRecordPath>,
) -> ApiResult {
    timeshift_manager.send(QueryTimeshiftRecordMessage {
        recorder_name: path.recorder.clone(),
        record_id: path.record,
    }).await?
        .map(WebTimeshiftRecord::from)
        .map(|record| actix_web::HttpResponse::Ok().json(record))
}

#[actix_web::get("/timeshift/{recorder}/stream")]
async fn get_timeshift_stream(
    config: actix_web::web::Data<Arc<Config>>,
    timeshift_manager: actix_web::web::Data<Addr<TimeshiftManagerActor>>,
    path: actix_web::web::Path<TimeshiftRecorderPath>,
    user: TunerUser,
    stream_query: actix_web::web::Query<TimeshiftStreamQuery>,
    filter_setting: FilterSetting,
) -> ApiResult {
    let recorder = timeshift_manager.send(QueryTimeshiftRecorderMessage {
        recorder_name: path.recorder.clone(),
    }).await??;

    let (stream, stop_trigger) = timeshift_manager.send(StartTimeshiftStreamingMessage {
        recorder_name: path.recorder.clone(),
        record_id: stream_query.record,
    }).await??;

    let data = mustache::MapBuilder::new()
        .insert_str("channel_name", &recorder.service.channel.name)
        .insert("channel_type", &recorder.service.channel.channel_type)?
        .insert_str("channel", &recorder.service.channel.channel)
        .insert("sid", &recorder.service.sid.value())?
        .build();

    let mut builder = FilterPipelineBuilder::new(data);
    builder.add_pre_filters(
        &config.pre_filters, &filter_setting.pre_filters)?;
    // The stream has already been decoded.
    builder.add_post_filters(
        &config.post_filters, &filter_setting.post_filters)?;
    let (filters, content_type) = builder.build();

    streaming(&config, user, stream, filters, content_type, stop_trigger).await
}

#[actix_web::get("/timeshift/{recorder}/records/{record}/stream")]
async fn get_timeshift_record_stream(
    req: actix_web::HttpRequest,
    config: actix_web::web::Data<Arc<Config>>,
    timeshift_manager: actix_web::web::Data<Addr<TimeshiftManagerActor>>,
    path: actix_web::web::Path<TimeshiftRecordPath>,
    user: TunerUser,
    filter_setting: FilterSetting,
) -> ApiResult {
    let recorder = timeshift_manager.send(QueryTimeshiftRecorderMessage {
        recorder_name: path.recorder.clone(),
    }).await??;

    let record = timeshift_manager.send(QueryTimeshiftRecordMessage{
        recorder_name: path.recorder.clone(),
        record_id: path.record,
    }).await??;

    let start_pos = req
        .headers()
        .get(actix_web::http::header::RANGE)
        .iter()
        .flat_map(|header| header.to_str().ok())
        .flat_map(|header| http_range::HttpRange::parse(header, record.size).ok())
        .flat_map(|ranges| ranges.iter().cloned().next())
        .map(|range| range.start)
        .next();

    let (stream, stop_trigger) = timeshift_manager.send(StartTimeshiftRecordStreamingMessage {
        recorder_name: path.recorder.clone(),
        record_id: path.record,
        start_pos,
    }).await??;

    let video_tags: Vec<u8> = record.program.video
        .iter()
        .map(|video| video.component_tag)
        .collect();

    let audio_tags: Vec<u8> = record.program.audios
        .values()
        .map(|audio| audio.component_tag)
        .collect();

    let duration = record.end_time - record.start_time;

    let data = mustache::MapBuilder::new()
        .insert_str("channel_name", &recorder.service.channel.name)
        .insert("channel_type", &recorder.service.channel.channel_type)?
        .insert_str("channel", &recorder.service.channel.channel)
        .insert("sid", &recorder.service.sid.value())?
        .insert("eid", &record.program.quad.eid())?
        .insert("video_tags", &video_tags)?
        .insert("audio_tags", &audio_tags)?
        .insert("id", &record.id)?
        .insert("duration", &duration.num_seconds())?
        .insert("size", &record.size)?
        .build();

    let mut builder = FilterPipelineBuilder::new(data);
    builder.add_pre_filters(
        &config.pre_filters, &filter_setting.pre_filters)?;
    // The stream has already been decoded.
    builder.add_post_filters(
        &config.post_filters, &filter_setting.post_filters)?;
    let (filters, content_type) = builder.build();

    streaming(&config, user, stream, filters, content_type, stop_trigger).await
}

#[actix_web::get("/iptv/playlist")]
async fn get_iptv_playlist(
    req: actix_web::HttpRequest,
    epg: actix_web::web::Data<Addr<EpgActor>>,
    mut filter_setting: FilterSetting,
) -> ApiResult {
    const INITIAL_BUFSIZE: usize = 8 * 1024;  // 8KB

    filter_setting.decode = true;  // always decode
    let query = serde_qs::to_string(&filter_setting).expect("Never fails");

    let services = epg.send(QueryServicesMessage).await??;

    let conn = req.connection_info();
    let mut buf = BytesMut::with_capacity(INITIAL_BUFSIZE);
    write!(buf, "#EXTM3U\n")?;
    for sv in services.iter() {
        let id = MirakurunServiceId::from(sv.triple());
        // The following format is compatible with EPGStation.
        // See API docs for the `/api/channel.m3u8` endpoint.
        //
        // U+3000 (IDEOGRAPHIC SPACE) at the end of each line is required for
        // avoiding garbled characters in `ＮＨＫＢＳプレミアム`.  Kodi or PVR
        // IPTV Simple Client seems to treat it as Latin-1 when removing U+3000.
        match sv.service_type {
            0x01 | 0xA1 | 0xA5 =>  // video
                write!(buf, "#EXTINF:-1 tvg-id=\"{}\" group-title=\"{}\", {}　\n",
                       id.value(), sv.channel.channel_type, sv.name)?,
            0x02 | 0xA2 | 0xA6 =>  // audio
                write!(buf, "#EXTINF:-1 tvg-id=\"{}\" group-title=\"{}-Radio\" radio=true, {}　\n",
                       id.value(), sv.channel.channel_type, sv.name)?,
            _ => unreachable!(),
        }
        write!(buf, "{}://{}/api/services/{}/stream?{}\n",
               conn.scheme(), conn.host(), id.value(), query)?;
    }

    Ok(actix_web::HttpResponse::Ok()
       .set_header("content-type", "application/x-mpegurl; charset=UTF-8")
       .body(buf))
}

#[actix_web::get("/iptv/epg")]
async fn get_iptv_epg(
    string_table: actix_web::web::Data<Arc<StringTable>>,
    epg: actix_web::web::Data<Addr<EpgActor>>,
    query: actix_web::web::Query<IptvEpgQuery>,
) -> ApiResult {
    const INITIAL_BUFSIZE: usize = 8 * 1024 * 1024;  // 8MB
    const DATETIME_FORMAT: &'static str = "%Y%m%d%H%M%S %z";

    let end_after = Jst::midnight();
    let start_before = end_after + chrono::Duration::days(query.days as i64);

    let services = epg.send(QueryServicesMessage).await??;
    let programs = epg.send(QueryProgramsMessage).await??;

    let mut buf = BytesMut::with_capacity(INITIAL_BUFSIZE);
    write!(buf, r#"<?xml version="1.0" encoding="UTF-8" ?>"#)?;
    write!(buf, r#"<!DOCTYPE tv SYSTEM "xmltv.dtd">"#)?;
    write!(buf, r#"<tv generator-info-name="{}">"#, escape(&server_name()))?;
    for sv in services.iter() {
        let id = MirakurunServiceId::from(sv.triple());
        write!(buf, r#"<channel id="{}"><display-name lang="ja">{}</display-name></channel>"#,
               id.value(), escape(&sv.name))?;
    }
    for pg in programs
        .iter()
        .filter(|pg| pg.name.is_some())
        .filter(|pg| pg.start_at < start_before && pg.end_at() > end_after)
    {
        let id = MirakurunServiceId::from(pg.quad);
        write!(buf, r#"<programme start="{}" stop="{}" channel="{}">"#,
               pg.start_at.format(DATETIME_FORMAT),
               pg.end_at().format(DATETIME_FORMAT),
               id.value())?;
        if let Some(name) = pg.name.as_ref() {
            write!(buf, r#"<title lang="ja">{}</title>"#, escape(&name))?;
        }
        if let Some(desc) = pg.description.as_ref() {
            write!(buf, r#"<desc lang="ja">"#)?;
            write!(buf, "{}", escape(&desc))?;
            if let Some(extended) = pg.extended.as_ref() {
                for (key, value) in extended.iter() {
                    if key.is_empty() {
                        write!(buf, "{}", escape(&value))?;
                    } else {
                        write!(buf, "\n{}\n{}", escape(&key), escape(&value))?;
                    }
                }
            }
            write!(buf, r#"</desc>"#)?;
        }
        if let Some(genres) = pg.genres.as_ref() {
            for genre in genres.iter() {
                let genre_str = &string_table.genres[genre.lv1 as usize].genre;
                let subgenre_str = &string_table.genres[genre.lv1 as usize]
                    .subgenres[genre.lv2 as usize];
                if subgenre_str.is_empty() {
                    write!(buf, r#"<category lang="ja">{}</category>"#,
                           escape(&genre_str))?;
                } else {
                    write!(buf, r#"<category lang="ja">{} / {}</category>"#,
                           escape(&genre_str), escape(&subgenre_str))?;
                }
            }
        }
        write!(buf, r#"</programme>"#)?;
    }
    write!(buf, r#"</tv>"#)?;

    Ok(actix_web::HttpResponse::Ok()
       .set_header("content-type", "application/xml; charset=UTF-8")
       .body(buf))
}

#[actix_web::get("/docs")]
async fn get_docs(
    config: actix_web::web::Data<Arc<Config>>,
) -> io::Result<actix_files::NamedFile> {
    // Mirakurun client requires this API since Mirakurun/2.14.0.
    //
    // mirakc simply returns a JSON data obtained from Mirakurun.
    Ok(actix_files::NamedFile::open(&config.mirakurun.openapi_json)?)
}

async fn do_get_service_stream(
    config: actix_web::web::Data<Arc<Config>>,
    tuner_manager: actix_web::web::Data<Addr<TunerManagerActor>>,
    channel: EpgChannel,
    sid: ServiceId,
    user: TunerUser,
    filter_setting: FilterSetting,
) -> ApiResult {
    let stream = tuner_manager.send(StartStreamingMessage {
        channel: channel.clone(),
        user: user.clone(),
    }).await??;

    // stop_trigger must be created here in order to stop streaming when an error occurs.
    let stop_trigger = TunerStreamStopTrigger::new(
        stream.id(), tuner_manager.get_ref().clone().recipient());

    let data = mustache::MapBuilder::new()
        .insert_str("channel_name", &channel.name)
        .insert("channel_type", &channel.channel_type)?
        .insert_str("channel", &channel.channel)
        .insert("sid", &sid.value())?
        .build();

    let mut builder = FilterPipelineBuilder::new(data);
    builder.add_pre_filters(
        &config.pre_filters, &filter_setting.pre_filters)?;
    if !stream.is_decoded() && filter_setting.decode {
        builder.add_decode_filter(&config.filters.decode_filter)?;
    }
    builder.add_service_filter(&config.filters.service_filter)?;
    builder.add_post_filters(
        &config.post_filters, &filter_setting.post_filters)?;
    let (filters, content_type) = builder.build();

    streaming(&config, user, stream, filters, content_type, stop_trigger).await
}

async fn streaming<T, S, D>(
    config: &Config,
    user: TunerUser,
    stream: MpegTsStream<T, S>,
    filters: Vec<String>,
    content_type: String,
    stop_triggers: D,
) -> ApiResult
where
    T: fmt::Display + Clone + Unpin + 'static,
    S: Stream<Item = io::Result<Bytes>> + Unpin + 'static,
    D: Unpin + 'static,
{
    let range = stream.range();
    if filters.is_empty() {
        do_streaming(
            user, stream, content_type, range, stop_triggers,
            config.server.stream_time_limit).await
    } else {
        log::debug!("Streaming with filters: {:?}", filters);

        let mut pipeline = spawn_pipeline(filters, stream.id())?;

        let (input, output) = pipeline.take_endpoints()?;

        let stream_id = stream.id();
        actix::spawn(async {
            let _ = stream.pipe(input).await;
        });

        // Use a MPSC channel as a buffer.
        //
        // The command pipeline often breaks when reading stops for a few
        // seconds.
        let mut stream = ChunkStream::new(
            output, config.server.stream_chunk_size);
        let (mut sender, receiver) =
            mpsc::channel(config.server.stream_max_chunks);
        actix::spawn(async move {
            while let Some(result) = stream.next().await {
                if let Ok(chunk) = result {
                    log::trace!("{}: Received a filtered chunk of {} bytes",
                                stream_id, chunk.len());
                    // The task yields if the buffer is full.
                    if let Err(_) = sender.send(Ok(chunk)).await {
                        log::debug!("{}: Disconnected by client", stream_id);
                        break;
                    }
                } else {
                    log::error!("{}: Error, stop streaming", stream_id);
                    break;
                }

                // Always yield for sending the chunk to the client quickly.
                //
                // The async task never yields voluntarily and can starve other
                // tasks waiting on the same executor.  For avoiding the
                // starvation, the task has to yields within a short term.
                //
                // Theoretically, one 32 KiB chunk comes every 10 ms.  This
                // period is a long enough time in the CPU time point of view.
                // Therefore, the async task simply yields at the end of every
                // iteration.
                tokio::task::yield_now().await;
            }

            drop(pipeline);
        });

        do_streaming(
            user, receiver, content_type, range, stop_triggers,
            config.server.stream_time_limit).await
    }
}

async fn do_streaming<S, D>(
    user: TunerUser,
    stream: S,
    content_type: String,
    range: Option<MpegTsStreamRange>,
    stop_trigger: D,
    time_limit: u64,
) -> ApiResult
where
    // actix_web::dev::HttpResponseBuilder::streaming() requires 'static...
    S: Stream<Item = io::Result<Bytes>> + Unpin + 'static,
    D: Unpin + 'static,
{
    let stream = MpegTsStreamTerminator::new(stream, stop_trigger);

    // No data is sent to the client until the first TS packet comes from the
    // streaming pipeline.
    let mut peekable = stream.peekable();
    let fut = Pin::new(&mut peekable).peek();
    match tokio::time::timeout(std::time::Duration::from_millis(time_limit), fut).await {
        Ok(None) => {
            // No packets come from the pipeline, maybe the program has been
            // canceled.
            Err(Error::ProgramNotFound)
        }
        Err(_) => {
            Err(Error::StreamingTimedOut)
        }
        Ok(_) =>  {
            // Send the response headers and start streaming.
            let mut builder = actix_web::HttpResponse::Ok();
            builder
                .force_close()
               .set_header("cache-control", "no-store")
               .set_header("content-type", content_type)
               .set_header("x-mirakurun-tuner-user-id", user.get_mirakurun_model().id);
            if let Some(range) = range {
                if range.is_partial() {
                    builder
                        .status(actix_web::http::StatusCode::PARTIAL_CONTENT);
                }
                builder
                    .set_header("accept-ranges", "bytes")
                    .set_header("content-range", range.make_content_range())
                    .no_chunking(range.bytes());
            }
            Ok(builder.streaming(peekable))
        }
    }
}

// extractors

#[derive(Deserialize)]
struct ChannelPath {
    channel_type: ChannelType,
    channel: String,
}

#[derive(Deserialize)]
struct ChannelServicePath {
    channel_type: ChannelType,
    channel: String,
    sid: ServiceId,
}

#[derive(Deserialize)]
struct ServicePath {
    id: MirakurunServiceId,
}

#[derive(Deserialize)]
struct ProgramPath {
    id: MirakurunProgramId,
}

#[derive(Deserialize)]
struct TimeshiftRecorderPath {
    recorder: String,
}

#[derive(Deserialize)]
struct TimeshiftRecordPath {
    recorder: String,
    record: TimeshiftRecordId,
}

#[derive(Deserialize)]
struct TimeshiftStreamQuery {
    #[serde(default)]
    record: Option<TimeshiftRecordId>,
}

// actix-web uses the serde_urlencoded crate for parsing the query in an URL.
// Unfortunately, the Vec support is out of scope for the serde_urlencoded
// crate and it's suggested to use the serde_qs crate.
//
// * nox/serde_urlencoded/issues/46
//
// actix-web tried to replace the serde_urlencoded crate with the serde_qs
// crate:
//
// * actix/actix-web/issues/38
// * actix/actix-web/issues/1211
//
// but the owner decided not to do that finally.  The reason is unclear at least
// for me.
//
// Actually, the serde_qs crate works well with actix-web without any
// difficulty as you can see in code below.
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
struct FilterSetting {
    #[serde(default)]
    #[serde(deserialize_with = "deserialize_stream_decode_query")]
    decode: bool,  // default: false
    #[serde(default)]
    pre_filters: Vec<String>,  // default: empty
    #[serde(default)]
    post_filters: Vec<String>,  // default: empty
}

impl FromRequest for FilterSetting {
    type Error = actix_web::Error;
    type Future = futures::future::Ready<Result<Self, Self::Error>>;
    type Config = ();

    fn from_request(
        req: &actix_web::HttpRequest,
        _: &mut actix_web::dev::Payload
    ) -> Self::Future {
        match serde_qs::from_str::<FilterSetting>(req.query_string()) {
            Ok(query) => futures::future::ok(query),
            Err(err) => futures::future::err(Error::from(err).into()),
        }
    }
}

fn deserialize_stream_decode_query<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    if s == "0" || s == "false" {
        return Ok(false);
    }
    if s == "1" || s == "true" {
        return Ok(true);
    }
    Err(serde::de::Error::custom(
        "The value of the decode query must be 0, 1, false or true"))
}

impl FromRequest for TunerUser {
    type Error = actix_web::Error;
    type Future = futures::future::Ready<Result<Self, Self::Error>>;
    type Config = ();

    fn from_request(
        req: &actix_web::HttpRequest,
        _: &mut actix_web::dev::Payload
    ) -> Self::Future {
        let id = req
            .connection_info()
            .realip_remote_addr()
            .map(|v| v.to_string())
            .unwrap_or_else(|| format!("unix:{}", ms_since_unix_epoch()));

        let agent = req
            .headers()
            .get_all(actix_web::http::header::USER_AGENT)
            .last()
            .map(|value| {
                value.to_str().ok().map_or(String::new(), |s| s.to_string())
            });

        let info = TunerUserInfo::Web { id, agent };

        let priority = req.headers().get_all("x-mirakurun-priority")
            .filter_map(|value| value.to_str().ok())
            .filter_map(|value| value.parse::<i32>().ok())
            .max()
            .map(|value| value.max(0))
            .map(TunerUserPriority::from)
            .unwrap_or_default();

        futures::future::ok(TunerUser { info, priority })
    }
}

fn ms_since_unix_epoch() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|dur| dur.as_millis())
        .unwrap_or(0)
}

#[derive(Deserialize)]
struct IptvEpgQuery {
    #[serde(default = "IptvEpgQuery::default_days")]
    days: u8,
}

impl IptvEpgQuery {
    fn default_days() -> u8 { 3 }
}

// middleware

struct AccessControl;

impl<S, B> actix_service::Transform<S> for AccessControl
where
    S: actix_service::Service<Request = actix_web::dev::ServiceRequest,
                              Response = actix_web::dev::ServiceResponse<B>,
                              Error = actix_web::Error>,
    S::Future: 'static,
    B: 'static,
{
    type Request = actix_web::dev::ServiceRequest;
    type Response = actix_web::dev::ServiceResponse<B>;
    type Error = actix_web::Error;
    type InitError = ();
    type Transform = AccessControlMiddleware<S>;
    type Future =
        futures::future::Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        futures::future::ok(AccessControlMiddleware(service))
    }
}

struct AccessControlMiddleware<S>(S);

impl<S, B> actix_service::Service for AccessControlMiddleware<S>
where
    S: actix_service::Service<Request = actix_web::dev::ServiceRequest,
                              Response = actix_web::dev::ServiceResponse<B>,
                              Error = actix_web::Error>,
    S::Future: 'static,
    B: 'static,
{
    type Request = actix_web::dev::ServiceRequest;
    type Response = actix_web::dev::ServiceResponse<B>;
    type Error = actix_web::Error;
    type Future = Pin<Box<dyn futures::future::Future<
            Output = Result<Self::Response, Self::Error>>>>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.0.poll_ready(cx)
    }

    fn call(&mut self, req: actix_web::dev::ServiceRequest) -> Self::Future {
        // Take no account of HTTP Forwarded and X-Forwarded-For headers.
        let ip = req.peer_addr().map(|socket| socket.ip());

        let allowed = match ip {
            Some(ip) => is_private_ip_addr(ip),
            None => true,  // UNIX domain socket
        };

        if allowed {
            Box::pin(self.0.call(req))
        } else {
            Box::pin(futures::future::ok(req.error_response(
                actix_web::error::ErrorForbidden(Error::AccessDenied))))
        }
    }
}

fn is_private_ip_addr(ip: IpAddr) -> bool {
    // TODO: IpAddr::is_global() is a nightly-only API at this point.
    match ip {
        IpAddr::V4(ip) => is_private_ipv4_addr(ip),
        IpAddr::V6(ip) => is_private_ipv6_addr(ip),
    }
}

fn is_private_ipv4_addr(ip: Ipv4Addr) -> bool {
    ip.is_loopback() || ip.is_private() || ip.is_link_local()
}

fn is_private_ipv6_addr(ip: Ipv6Addr) -> bool {
    // TODO: Support only IPv4-compatible and IPv4-mapped addresses at this
    //       moment.
    match ip.to_ipv4() {
        Some(ip) => is_private_ipv4_addr(ip),
        None => false,
    }
}

// Took from https://github.com/rust-lang/rust/blob/master/src/librustdoc/html/escape.rs

#[inline(always)]
fn escape<'a>(str: &'a str) -> Escape<'a> {
    Escape(str)
}

struct Escape<'a>(pub &'a str);

impl<'a> std::fmt::Display for Escape<'a> {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Because the internet is always right, turns out there's not that many
        // characters to escape: http://stackoverflow.com/questions/7381974
        let Escape(s) = *self;
        let pile_o_bits = s;
        let mut last = 0;
        for (i, ch) in s.bytes().enumerate() {
            match ch as char {
                '<' | '>' | '&' | '\'' | '"' => {
                    fmt.write_str(&pile_o_bits[last..i])?;
                    let s = match ch as char {
                        '>' => "&gt;",
                        '<' => "&lt;",
                        '&' => "&amp;",
                        '\'' => "&#39;",
                        '"' => "&quot;",
                        _ => unreachable!(),
                    };
                    fmt.write_str(s)?;
                    last = i + 1;
                }
                _ => {}
            }
        }

        if last < s.len() {
            fmt.write_str(&pile_o_bits[last..])?;
        }
        Ok(())
    }
}

// data models

// timeshift record

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebTimeshiftRecorder {
    name: String,
    service: MirakurunService,
    #[serde(with = "serde_jst")]
    start_time: DateTime<Jst>,
    #[serde(with = "serde_duration_in_millis")]
    duration: Duration,
    pipeline: Vec<WebProcessModel>,
    recording: bool,
}

impl From<TimeshiftRecorderModel> for WebTimeshiftRecorder {
    fn from(model: TimeshiftRecorderModel) -> Self {
        Self {
            name: model.name,
            service: model.service.into(),
            start_time: model.start_time.clone(),
            duration: model.end_time - model.start_time,
            pipeline: model.pipeline.into_iter().map(WebProcessModel::from).collect(),
            recording: model.recording,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebProcessModel {
    command: String,
    pid: u32,
}

impl From<CommandPipelineProcessModel> for WebProcessModel {
    fn from(model: CommandPipelineProcessModel) -> Self {
        Self {
            command: model.command,
            pid: model.pid,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebTimeshiftRecord {
    id: TimeshiftRecordId,
    program: MirakurunProgram,
    #[serde(with = "serde_jst")]
    start_time: DateTime<Jst>,
    #[serde(with = "serde_duration_in_millis")]
    duration: Duration,
    size: u64,
    recording: bool,
}

impl From<TimeshiftRecordModel> for WebTimeshiftRecord {
    fn from(model: TimeshiftRecordModel) -> Self {
        Self {
            id: model.id,
            program: model.program.into(),
            start_time: model.start_time.clone(),
            duration: model.end_time - model.start_time,
            size: model.size,
            recording: model.recording,
        }
    }
}

// tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use actix_http;
    use assert_matches::*;
    use crate::broadcaster::BroadcasterStream;

    async fn request(req: actix_http::Request) -> actix_web::HttpResponse {
        let mut app = actix_web::test::init_service(
            actix_web::App::new()
                .data(config_for_test())
                .data(tuner_manager_for_test())
                .data(epg_for_test())
                .data(timeshift_manager_for_test())
                .wrap(AccessControl)
                .service(create_api_service())).await;
        actix_web::test::call_service(&mut app, req).await.into()
    }

    async fn get(uri: &str) -> actix_web::HttpResponse {
        let req = actix_web::test::TestRequest::with_uri(uri)
            .method(actix_web::http::Method::GET)
            .to_request();
        request(req).await
    }

    async fn get_with_peer_addr(
        uri: &str,
        addr: SocketAddr
    ) -> actix_web::HttpResponse {
        let req = actix_web::test::TestRequest::with_uri(uri)
            .method(actix_web::http::Method::GET)
            .peer_addr(addr)
            .to_request();
        request(req).await
    }

    #[actix_rt::test]
    async fn test_get_unknown() {
        let res = get("/api/unknown").await;
        assert!(res.status() == actix_web::http::StatusCode::NOT_FOUND);
    }

    #[actix_rt::test]
    async fn test_get_version() {
        let res = get("/api/version").await;
        assert!(res.status() == actix_web::http::StatusCode::OK);
    }

    #[actix_rt::test]
    async fn test_get_status() {
        let res = get("/api/status").await;
        assert!(res.status() == actix_web::http::StatusCode::OK);
    }

    #[actix_rt::test]
    async fn test_get_channels() {
        let res = get("/api/channels").await;
        assert!(res.status() == actix_web::http::StatusCode::OK);
    }

    #[actix_rt::test]
    async fn test_get_services() {
        let res = get("/api/services").await;
        assert!(res.status() == actix_web::http::StatusCode::OK);
    }

    #[actix_rt::test]
    async fn test_get_service() {
        let res = get("/api/services/1").await;
        assert!(res.status() == actix_web::http::StatusCode::OK);

        let res = get("/api/services/0").await;
        assert!(res.status() == actix_web::http::StatusCode::NOT_FOUND);
    }

    #[actix_rt::test]
    async fn test_get_programs() {
        let res = get("/api/programs").await;
        assert!(res.status() == actix_web::http::StatusCode::OK);
    }

    #[actix_rt::test]
    async fn test_get_program() {
        let res = get("/api/programs/1").await;
        assert!(res.status() == actix_web::http::StatusCode::OK);

        let res = get("/api/programs/0").await;
        assert!(res.status() == actix_web::http::StatusCode::NOT_FOUND);
    }

    #[actix_rt::test]
    async fn test_get_tuners() {
        let res = get("/api/tuners").await;
        assert!(res.status() == actix_web::http::StatusCode::OK);
    }

    #[actix_rt::test]
    async fn test_get_channel_stream() {
        let res = get("/api/channels/GR/ch/stream").await;
        assert!(res.status() == actix_web::http::StatusCode::OK);
        assert!(res.headers().contains_key("x-mirakurun-tuner-user-id"));

        let res = get("/api/channels/GR/0/stream").await;
        assert!(res.status() == actix_web::http::StatusCode::NOT_FOUND);

        let res = get("/api/channels/WOWOW/ch/stream").await;
        assert!(res.status() == actix_web::http::StatusCode::NOT_FOUND);

        let res = get("/api/channels/GR/xx/stream").await;
        assert!(res.status() == actix_web::http::StatusCode::NOT_FOUND);

        let decode_values = [0, 1];

        let valid_pairs =
            ["GR", "BS", "CS", "SKY"].iter().zip(&decode_values);
        for (channel_type, decode) in valid_pairs {
            let res = get(format!("/api/channels/{}/ch/stream?decode={}",
                                  channel_type, decode).as_str()).await;
            assert!(res.status() == actix_web::http::StatusCode::OK);
        }

        for decode in &decode_values {
            let res = get(format!("/api/channels/WOWOW/ch/stream?decode={}",
                                  decode).as_str()).await;
            assert!(res.status() == actix_web::http::StatusCode::NOT_FOUND);
        }
    }

    #[actix_rt::test]
    async fn test_get_channel_service_stream() {
        let res = get("/api/channels/GR/ch/services/1/stream").await;
        assert!(res.status() == actix_web::http::StatusCode::OK);
        assert!(res.headers().contains_key("x-mirakurun-tuner-user-id"));

        let res = get("/api/channels/GR/0/services/1/stream").await;
        assert!(res.status() == actix_web::http::StatusCode::NOT_FOUND);

        let res = get("/api/channels/WOWOW/ch/services/1/stream").await;
        assert!(res.status() == actix_web::http::StatusCode::NOT_FOUND);

        let decode_values = [0, 1];

        let valid_pairs =
            ["GR", "BS", "CS", "SKY"].iter().zip(&decode_values);
        for (channel_type, decode) in valid_pairs {
            let res = get(format!(
                "/api/channels/{}/ch/services/1/stream?decode={}",
                channel_type, decode).as_str()).await;
            assert!(res.status() == actix_web::http::StatusCode::OK);
        }

        for decode in &decode_values {
            let res = get(format!(
                "/api/channels/WOWOW/ch/services/1/stream?decode={}",
                decode).as_str()).await;
            assert!(res.status() == actix_web::http::StatusCode::NOT_FOUND);
        }

        for decode in &decode_values {
            let res = get(format!(
                "/api/channels/WOWOW/ch/services/2/stream?decode={}",
                decode).as_str()).await;
            assert!(res.status() == actix_web::http::StatusCode::NOT_FOUND);
        }
    }

    #[actix_rt::test]
    async fn test_get_service_stream() {
        let res = get("/api/services/1/stream").await;
        assert!(res.status() == actix_web::http::StatusCode::OK);
        assert!(res.headers().contains_key("x-mirakurun-tuner-user-id"));

        let res = get("/api/services/0/stream").await;
        assert!(res.status() == actix_web::http::StatusCode::NOT_FOUND);

        let decode_values = [0, 1];

        for decode in &decode_values {
            let res = get(format!("/api/services/1/stream?decode={}",
                                  decode).as_str()).await;
            assert!(res.status() == actix_web::http::StatusCode::OK);
        }

        for decode in &decode_values {
            let res = get(format!("/api/services/0/stream?decode={}",
                                  decode).as_str()).await;
            assert!(res.status() == actix_web::http::StatusCode::NOT_FOUND);
        }

        for decode in &decode_values {
            let res = get(format!("/api/services/2/stream?decode={}",
                                  decode).as_str()).await;
            assert!(res.status() == actix_web::http::StatusCode::NOT_FOUND);
        }
    }

    #[actix_rt::test]
    async fn test_get_program_stream() {
        let res = get("/api/programs/100001/stream").await;
        assert!(res.status() == actix_web::http::StatusCode::OK);
        assert!(res.headers().contains_key("x-mirakurun-tuner-user-id"));

        let res = get("/api/programs/0/stream").await;
        assert!(res.status() == actix_web::http::StatusCode::NOT_FOUND);

        let decode_values = [0, 1];

        for decode in &decode_values {
            let res = get(format!("/api/programs/100001/stream?decode={}",
                                  decode).as_str()).await;
            assert!(res.status() == actix_web::http::StatusCode::OK);
        }

        for decode in &decode_values {
            let res = get(format!("/api/programs/0/stream?decode={}",
                                  decode).as_str()).await;
            assert!(res.status() == actix_web::http::StatusCode::NOT_FOUND);
        }

        for decode in &decode_values {
            let res = get(format!("/api/programs/200001/stream?decode={}",
                                  decode).as_str()).await;
            assert!(res.status() == actix_web::http::StatusCode::NOT_FOUND);
        }
    }

    #[actix_rt::test]
    async fn test_get_timeshift_recorders() {
        let res = get("/api/timeshift").await;
        assert!(res.status() == actix_web::http::StatusCode::OK);
    }

    #[actix_rt::test]
    async fn test_get_timeshift_recorder() {
        let res = get("/api/timeshift/test").await;
        assert!(res.status() == actix_web::http::StatusCode::OK);

        let res = get("/api/timeshift/not_found").await;
        assert!(res.status() == actix_web::http::StatusCode::NOT_FOUND);
    }

    #[actix_rt::test]
    async fn test_get_timeshift_records() {
        let res = get("/api/timeshift/test/records").await;
        assert!(res.status() == actix_web::http::StatusCode::OK);
    }

    #[actix_rt::test]
    async fn test_get_timeshift_record() {
        let res = get("/api/timeshift/test/records/0").await;
        assert!(res.status() == actix_web::http::StatusCode::OK);

        let res = get("/api/timeshift/test/records/1").await;
        assert!(res.status() == actix_web::http::StatusCode::NOT_FOUND);
    }

    #[actix_rt::test]
    async fn test_get_timeshift_stream() {
        let res = get("/api/timeshift/test/stream").await;
        assert!(res.status() == actix_web::http::StatusCode::OK);
        assert!(res.headers().contains_key("x-mirakurun-tuner-user-id"));

        let res = get("/api/timeshift/not_found/stream").await;
        assert!(res.status() == actix_web::http::StatusCode::NOT_FOUND);
    }

    #[actix_rt::test]
    async fn test_get_timeshift_record_stream() {
        let res = get("/api/timeshift/test/records/0/stream").await;
        assert!(res.status() == actix_web::http::StatusCode::OK);
        assert!(res.headers().contains_key("x-mirakurun-tuner-user-id"));

        let res = get("/api/timeshift/not_found/records/0/stream").await;
        assert!(res.status() == actix_web::http::StatusCode::NOT_FOUND);
    }

    #[actix_rt::test]
    async fn test_get_timeshift_program_stream() {
        // TODO
    }

    #[actix_rt::test]
    async fn test_get_iptv_playlist() {
        let res = get("/api/iptv/playlist").await;
        assert!(res.status() == actix_web::http::StatusCode::OK);
    }

    #[actix_rt::test]
    async fn test_get_docs() {
        let res = get("/api/docs").await;
        assert!(res.status() == actix_web::http::StatusCode::OK);
    }

    #[actix_rt::test]
    async fn test_access_control() {
        let res = get_with_peer_addr(
            "/api/version", "127.0.0.1:10000".parse().unwrap()).await;
        assert_eq!(res.status(), actix_web::http::StatusCode::OK);

        let res = get_with_peer_addr(
            "/api/version", "8.8.8.8:10000".parse().unwrap()).await;
        assert_eq!(res.status(), actix_web::http::StatusCode::FORBIDDEN);
    }

    #[test]
    fn test_is_private_ip_addr() {
        assert!(is_private_ip_addr("127.0.0.1".parse().unwrap()));
        assert!(is_private_ip_addr("10.0.0.1".parse().unwrap()));
        assert!(is_private_ip_addr("172.16.0.1".parse().unwrap()));
        assert!(is_private_ip_addr("192.168.0.1".parse().unwrap()));
        assert!(!is_private_ip_addr("8.8.8.8".parse().unwrap()));
    }

    #[actix_rt::test]
    async fn test_do_streaming() {
        let user = user_for_test(0.into());

        let result = do_streaming(
            user.clone(), futures::stream::empty(), "video/MP2T".to_string(), None, (),
            1000).await;
        assert_matches!(result, Err(Error::ProgramNotFound));

        let result = do_streaming(
            user.clone(),  futures::stream::pending(), "video/MP2T".to_string(), None, (),
            1).await;
        assert_matches!(result, Err(Error::StreamingTimedOut));
    }

    #[actix_rt::test]
    async fn test_filter_setting() {
        async fn do_test(
            query: &str
        ) -> Result<FilterSetting, <FilterSetting as FromRequest>::Error> {
            let uri = format!("/stream{}", query);
            let (req, mut payload) = actix_web::test::TestRequest::with_uri(&uri)
                .method(actix_web::http::Method::GET)
                .to_http_parts();
            FilterSetting::from_request(&req, &mut payload).await
        }

        assert_matches!(do_test("").await, Ok(v) => {
            assert!(!v.decode);
            assert!(v.pre_filters.is_empty());
            assert!(v.post_filters.is_empty());
        });

        assert_matches!(do_test("?unknown=0").await, Ok(v) => {
            assert!(!v.decode);
            assert!(v.pre_filters.is_empty());
            assert!(v.post_filters.is_empty());
        });

        assert_matches!(do_test("?decode=0").await, Ok(v) => {
            assert!(!v.decode);
            assert!(v.pre_filters.is_empty());
            assert!(v.post_filters.is_empty());
        });

        assert_matches!(do_test("?decode=1").await, Ok(v) => {
            assert!(v.decode);
            assert!(v.pre_filters.is_empty());
            assert!(v.post_filters.is_empty());
        });

        assert_matches!(do_test("?decode=false").await, Ok(v) => {
            assert!(!v.decode);
            assert!(v.pre_filters.is_empty());
            assert!(v.post_filters.is_empty());
        });

        assert_matches!(do_test("?decode=true").await, Ok(v) => {
            assert!(v.decode);
            assert!(v.pre_filters.is_empty());
            assert!(v.post_filters.is_empty());
        });

        assert_matches!(do_test("?decode=x").await, Err(_));

        assert_matches!(do_test("?pre-filters[]=a").await, Ok(v) => {
            assert!(!v.decode);
            assert_eq!(v.pre_filters.len(), 1);
            assert_eq!(v.pre_filters[0], "a".to_string());
            assert!(v.post_filters.is_empty());
        });

        assert_matches!(do_test("?pre-filters[]=a&pre-filters[]=b").await, Ok(v) => {
            assert!(!v.decode);
            assert_eq!(v.pre_filters.len(), 2);
            assert_eq!(v.pre_filters[0], "a".to_string());
            assert_eq!(v.pre_filters[1], "b".to_string());
            assert!(v.post_filters.is_empty());
        });

        assert_matches!(do_test("?pre-filters[0]=a").await, Ok(v) => {
            assert!(!v.decode);
            assert_eq!(v.pre_filters.len(), 1);
            assert_eq!(v.pre_filters[0], "a".to_string());
            assert!(v.post_filters.is_empty());
        });

        assert_matches!(do_test("?pre-filters[0]=a&pre-filters[1]=b").await, Ok(v) => {
            assert!(!v.decode);
            assert_eq!(v.pre_filters.len(), 2);
            assert_eq!(v.pre_filters[0], "a".to_string());
            assert_eq!(v.pre_filters[1], "b".to_string());
            assert!(v.post_filters.is_empty());
        });

        assert_matches!(do_test("?pre-filters[1]=a").await, Ok(v) => {
            assert!(!v.decode);
            assert_eq!(v.pre_filters.len(), 1);
            assert_eq!(v.pre_filters[0], "a".to_string());
            assert!(v.post_filters.is_empty());
        });

        assert_matches!(do_test("?pre-filters[1]=a&pre-filters[2]=b").await, Ok(v) => {
            assert!(!v.decode);
            assert_eq!(v.pre_filters.len(), 2);
            assert_eq!(v.pre_filters[0], "a".to_string());
            assert_eq!(v.pre_filters[1], "b".to_string());
            assert!(v.post_filters.is_empty());
        });

        assert_matches!(do_test("?pre-filters=a").await, Err(_));
        assert_matches!(do_test("?pre-filters[x]=a").await, Err(_));
        assert_matches!(do_test("?pre-filters[0]=a&pre-filters[0]=b").await, Err(_));

        assert_matches!(do_test("?decode=1&pre-filters[]=a&post-filters[]=b").await, Ok(v) => {
            assert!(v.decode);
            assert_eq!(v.pre_filters.len(), 1);
            assert_eq!(v.pre_filters[0], "a".to_string());
            assert_eq!(v.post_filters.len(), 1);
            assert_eq!(v.post_filters[0], "b".to_string());
        });

        assert_matches!(do_test("?pre-filters[]=a&decode=1&post-filters[]=b").await, Ok(v) => {
            assert!(v.decode);
            assert_eq!(v.pre_filters.len(), 1);
            assert_eq!(v.pre_filters[0], "a".to_string());
            assert_eq!(v.post_filters.len(), 1);
            assert_eq!(v.post_filters[0], "b".to_string());
        });
    }

    #[test]
    fn test_escape() {
        assert_eq!("a&lt;a&gt;a&amp;a&#39;a&quot;a", format!("{}", escape(r#"a<a>a&a'a"a"#)));
    }

    fn config_for_test() -> Arc<Config> {
        let mut config = Config::default();
        // Disable all filters
        config.filters.service_filter = Default::default();
        config.filters.program_filter = Default::default();
        // Disable tracking airtime
        config.recorder.track_airtime_command = "true".to_string();
        // "/dev/null" is enough to test
        config.mirakurun.openapi_json = "/dev/null".to_string();

        Arc::new(config)
    }

    fn tuner_manager_for_test() -> Addr<TunerManagerActor> {
        TunerManagerActor::mock(Box::new(|msg, _ctx| {
            if let Some(_) = msg.downcast_ref::<QueryTunersMessage>() {
                Box::<Option<Result<Vec<MirakurunTuner>, Error>>>::new(
                    Some(Ok(Vec::new())))
            } else if let Some(msg) = msg.downcast_ref::<StartStreamingMessage>() {
                if msg.channel.channel == "ch" {
                    let (mut tx, stream) = BroadcasterStream::new_for_test();
                    let _ = tx.try_send(Bytes::from("hi"));
                    let result = Ok(MpegTsStream::new(TunerSubscriptionId::default(), stream));
                    Box::<Option<Result<_, Error>>>::new(Some(result))
                } else {
                    let (_, stream) = BroadcasterStream::new_for_test();
                    let result = Ok(MpegTsStream::new(TunerSubscriptionId::default(), stream));
                    Box::<Option<Result<_, Error>>>::new(Some(result))
                }
            } else if let Some(_) = msg.downcast_ref::<StopStreamingMessage>() {
                Box::new(Some(()))
            } else {
                unimplemented!();
            }
        })).start()
    }

    fn epg_for_test() -> Addr<EpgActor> {
        EpgActor::mock(Box::new(|msg, _| {
            if let Some(_) = msg.downcast_ref::<QueryChannelsMessage>() {
                Box::<Option<Result<Vec<MirakurunChannel>, Error>>>::new(
                    Some(Ok(Vec::new())))
            } else if let Some(msg) = msg.downcast_ref::<QueryChannelMessage>() {
                let result = if msg.channel == "0" {
                    Err(Error::ChannelNotFound)
                } else {
                    Ok(EpgChannel {
                        name: "test".to_string(),
                        channel_type: msg.channel_type,
                        channel: msg.channel.clone(),
                        extra_args: "".to_string(),
                        services: Vec::new(),
                        excluded_services: Vec::new(),
                    })
                };
                Box::<Option<Result<EpgChannel, Error>>>::new(Some(result))
            } else if let Some(_) = msg.downcast_ref::<QueryServicesMessage>() {
                Box::<Option<Result<Vec<EpgService>, Error>>>::new(
                    Some(Ok(Vec::new())))
            } else if let Some(msg) = msg.downcast_ref::<QueryServiceMessage>() {
                let result = match msg {
                    QueryServiceMessage::ByNidSid { nid, sid } => {
                        if sid.value() == 0 {
                            Err(Error::ServiceNotFound)
                        } else {
                            let channel = if sid.value() == 1 {
                                "ch"
                            } else {
                                ""
                            };
                            Ok(EpgService {
                                nid: *nid,
                                tsid: 0.into(),
                                sid: *sid,
                                service_type: 1,
                                logo_id: 0,
                                remote_control_key_id: 0,
                                name: "test".to_string(),
                                channel: EpgChannel {
                                    name: "test".to_string(),
                                    channel_type: ChannelType::GR,
                                    channel: channel.to_string(),
                                    extra_args: "".to_string(),
                                    services: Vec::new(),
                                    excluded_services: Vec::new(),
                                },
                            })
                        }
                    }
                };
                Box::<Option<Result<EpgService, Error>>>::new(Some(result))
            } else if let Some(msg) = msg.downcast_ref::<QueryClockMessage>() {
                let result = match msg.triple.sid().value() {
                    0 => Err(Error::ClockNotSynced),
                    _ => Ok(Clock { pid: 0, pcr: 0, time: 0 }),
                };
                Box::<Option<Result<Clock, Error>>>::new(Some(result))
            } else if let Some(_) = msg.downcast_ref::<QueryProgramsMessage>() {
                Box::<Option<Result<Vec<EpgProgram>, Error>>>::new(
                    Some(Ok(Vec::new())))
            } else if let Some(msg) = msg.downcast_ref::<QueryProgramMessage>() {
                let result = match msg {
                    QueryProgramMessage::ByNidSidEid { nid, sid, eid } => {
                        if eid.value() == 0 {
                            Err(Error::ProgramNotFound)
                        } else {
                            Ok(EpgProgram::new(
                                (*nid, 0.into(), *sid, *eid).into()))
                        }
                    }
                };
                Box::<Option<Result<EpgProgram, Error>>>::new(Some(result))
            } else if let Some(_) = msg.downcast_ref::<RemoveAirtimeMessage>() {
                Box::<Option<()>>::new(Some(()))
            } else {
                unimplemented!();
            }
        })).start()
    }

    fn timeshift_manager_for_test() -> Addr<TimeshiftManagerActor> {
        TimeshiftManagerActor::mock(Box::new(|msg, _ctx| {
            if let Some(_) = msg.downcast_ref::<QueryTimeshiftRecordersMessage>() {
                Box::<Option<Result<Vec<TimeshiftRecorderModel>, Error>>>::new(
                    Some(Ok(Vec::new())))
            } else if let Some(msg) = msg.downcast_ref::<QueryTimeshiftRecorderMessage>() {
                let result = if msg.recorder_name == "test" {
                    Ok(TimeshiftRecorderModel {
                        name: "test".to_string(),
                        service: EpgService {
                            nid: 1.into(),
                            tsid: 2.into(),
                            sid: 3.into(),
                            service_type: 1,
                            logo_id: 0,
                            remote_control_key_id: 0,
                            name: "test".to_string(),
                            channel: EpgChannel {
                                name: "test".to_string(),
                                channel_type: ChannelType::GR,
                                channel: "test".to_string(),
                                extra_args: "".to_string(),
                                services: Vec::new(),
                                excluded_services: Vec::new(),
                            },
                        },
                        start_time: Jst::now(),
                        end_time: Jst::now(),
                        pipeline: vec![],
                        recording: true,
                    })
                } else {
                    Err(Error::RecordNotFound)
                };
                Box::<Option<Result<_, Error>>>::new(Some(result))
            } else if let Some(_) = msg.downcast_ref::<QueryTimeshiftRecordsMessage>() {
                Box::<Option<Result<Vec<TimeshiftRecordModel>, Error>>>::new(
                    Some(Ok(Vec::new())))
            } else if let Some(msg) = msg.downcast_ref::<QueryTimeshiftRecordMessage>() {
                let result = if msg.record_id == 0.into() {
                    Ok(TimeshiftRecordModel {
                        id: 0.into(),
                        program: EpgProgram::new((0, 0, 0, 0).into()),
                        start_time: Jst::now(),
                        end_time: Jst::now(),
                        size: 0,
                        recording: true,
                    })
                } else {
                    Err(Error::RecordNotFound)
                };
                Box::<Option<Result<_, Error>>>::new(Some(result))
            } else if let Some(msg) = msg.downcast_ref::<StartTimeshiftStreamingMessage>() {
                let result = if msg.recorder_name == "test" {
                    let (reader, stop_trigger) = TimeshiftFileReader::open_for_test();
                    let stream = ChunkStream::new_for_test(reader);
                    Ok((MpegTsStream::new("".to_string(), stream), stop_trigger))
                } else {
                    Err(Error::NoContent)
                };
                Box::<Option<Result<_, Error>>>::new(Some(result))
            } else if let Some(msg) = msg.downcast_ref::<StartTimeshiftRecordStreamingMessage>() {
                use tokio::io::AsyncReadExt;
                let result = if msg.recorder_name == "test" {
                    let (reader, stop_trigger) = TimeshiftFileReader::open_for_test();
                    let stream = ChunkStream::new_for_test(reader.take(1));
                    Ok((MpegTsStream::new("".to_string(), stream), stop_trigger))
                } else {
                    Err(Error::NoContent)
                };
                Box::<Option<Result<_, Error>>>::new(Some(result))
            } else {
                unimplemented!();
            }
        })).start()
    }

    fn user_for_test(priority: TunerUserPriority) -> TunerUser {
        TunerUser {
            info: TunerUserInfo::Web { id: "".to_string(), agent: None },
            priority
        }
    }
}
