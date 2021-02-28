use std::fmt;

use chrono::{DateTime, Duration};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::datetime_ext::{serde_jst, serde_duration_in_millis, Jst};
use crate::eit_feeder::{AudioComponentDescriptor, ComponentDescriptor };
use crate::epg::{EpgChannel, EpgService, EpgProgram};
use crate::tuner::TunerSubscriptionId;

#[derive(Clone, Copy, Debug, PartialEq)]
#[derive(Deserialize, Serialize)]
pub enum ChannelType { GR, BS, CS, SKY }

impl fmt::Display for ChannelType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            ChannelType::GR => write!(f, "GR"),
            ChannelType::BS => write!(f, "BS"),
            ChannelType::CS => write!(f, "CS"),
            ChannelType::SKY => write!(f, "SKY"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[derive(Deserialize, Serialize)]
pub struct NetworkId(u16);

impl NetworkId {
    pub fn value(&self) -> u16 { self.0 }
}

impl fmt::Display for NetworkId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NID#{:04X}", self.0)
    }
}

impl From<u16> for NetworkId {
    fn from(value: u16) -> Self { Self(value) }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[derive(Deserialize, Serialize)]
pub struct TransportStreamId(u16);

impl TransportStreamId {
    pub fn value(&self) -> u16 { self.0 }
}

impl fmt::Display for TransportStreamId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TSID#{:04X}", self.0)
    }
}

impl From<u16> for TransportStreamId {
    fn from(value: u16) -> Self { Self(value) }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[derive(Deserialize, Serialize)]
pub struct ServiceId(u16);

impl ServiceId {
    pub fn value(&self) -> u16 { self.0 }
}

impl fmt::Display for ServiceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SID#{:04X}", self.0)
    }
}

impl From<u16> for ServiceId {
    fn from(value: u16) -> Self { Self(value) }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[derive(Deserialize, Serialize)]
pub struct EventId(u16);

impl EventId {
    pub fn value(&self) -> u16 { self.0 }
}

impl fmt::Display for EventId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "EID#{:04X}", self.0)
    }
}

impl From<u16> for EventId {
    fn from(value: u16) -> Self { Self(value) }
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
#[derive(Deserialize, Serialize)]
#[cfg_attr(test, derive(Debug))]
pub struct ServiceTriple(u64);

impl ServiceTriple {
    pub fn new(
        nid: NetworkId,
        tsid: TransportStreamId,
        sid: ServiceId,
    ) -> Self {
        ServiceTriple(
            (nid.value() as u64)  << 48 |
            (tsid.value() as u64) << 32 |
            (sid.value() as u64)  << 16 )
    }

    pub fn value(&self) -> u64 {
        self.0
    }

    pub fn nid(&self) -> NetworkId {
        NetworkId(((self.value() >> 48) & 0xFFFF) as u16)
    }

    pub fn tsid(&self) -> TransportStreamId {
        TransportStreamId(((self.value() >> 32) & 0xFFFF) as u16)
    }

    pub fn sid(&self) -> ServiceId {
        ServiceId(((self.value() >> 16) & 0xFFFF) as u16)
    }
}

impl fmt::Display for ServiceTriple {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:016X}", self.value())
    }
}

impl From<(NetworkId, TransportStreamId, ServiceId)> for ServiceTriple {
    fn from(
        triple: (NetworkId, TransportStreamId, ServiceId)
    ) -> ServiceTriple {
        ServiceTriple::new(triple.0, triple.1, triple.2)
    }
}

impl From<EventQuad> for ServiceTriple {
    fn from(quad: EventQuad) -> Self {
        ServiceTriple::new(quad.nid(), quad.tsid(), quad.sid())
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[derive(Deserialize, Serialize)]
pub struct EventQuad(u64);

impl EventQuad {
    pub fn new(
        nid: NetworkId,
        tsid: TransportStreamId,
        sid: ServiceId,
        eid: EventId
    ) -> Self {
        EventQuad(
            (nid.value() as u64)  << 48 |
            (tsid.value() as u64) << 32 |
            (sid.value() as u64)  << 16 |
            (eid.value() as u64)
        )
    }

    pub fn value(&self) -> u64 {
        self.0
    }

    pub fn nid(&self) -> NetworkId {
        NetworkId(((self.value() >> 48) & 0xFFFF) as u16)
    }

    pub fn tsid(&self) -> TransportStreamId {
        TransportStreamId(((self.value() >> 32) & 0xFFFF) as u16)
    }

    pub fn sid(&self) -> ServiceId {
        ServiceId(((self.value() >> 16) & 0xFFFF) as u16)
    }

    pub fn eid(&self) -> EventId {
        EventId((self.value() & 0xFFFF) as u16)
    }
}

impl fmt::Display for EventQuad {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:016X}", self.value())
    }
}

impl From<(ServiceTriple, EventId)> for EventQuad {
    fn from(tuple: (ServiceTriple, EventId)) -> EventQuad {
        EventQuad::new(tuple.0.nid(), tuple.0.tsid(), tuple.0.sid(), tuple.1)
    }
}

impl From<(NetworkId, TransportStreamId, ServiceId, EventId)> for EventQuad {
    fn from(
        quad: (NetworkId, TransportStreamId, ServiceId, EventId)
    ) -> EventQuad {
        EventQuad::new(quad.0, quad.1, quad.2, quad.3)
    }
}

#[derive(Clone, Debug)]
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Clock {
    // PID of PCR packets
    pub pid: u16,
    // 27MHz, 42bits PCR value corresponding to the time
    pub pcr: i64,
    // UNIX time in ms
    pub time: i64,
}

#[derive(Clone, Debug)]
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EpgGenre {
    pub lv1: u8,
    pub lv2: u8,
    pub un1: u8,
    pub un2: u8,
}

impl EpgGenre {
    pub fn new((lv1, lv2, un1, un2): (u8, u8, u8, u8)) -> EpgGenre {
        EpgGenre { lv1, lv2, un1, un2 }
    }
}

// user

#[derive(Clone)]
pub enum TunerUserInfo {
    Job { name: String },
    Recorder { name: String },
    Tracker { stream_id: TunerSubscriptionId },
    Web { id: String, agent: Option<String> },
}

impl TunerUserInfo {
    fn get_mirakurun_model(&self) -> (String, Option<String>) {
        match self {
            Self::Job { name } => (format!("job:{}", name), None),
            Self::Recorder { name } => (format!("recorder:{}", name), None),
            Self::Tracker { stream_id } => (format!("tracker:{}", stream_id), None),
            Self::Web { id, agent } => (id.clone(), agent.clone()),
        }
    }
}

impl fmt::Display for TunerUserInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Job { name } => write!(f, "Job({})", name),
            Self::Recorder { name } => write!(f, "Recorder({})", name),
            Self::Tracker { stream_id } =>
                write!(f, "Tracker({})", stream_id),
            Self::Web { id, agent: None } =>
                write!(f, r#"Web(id="{}")"#, id),
            Self::Web { id, agent: Some(agent) } =>
                write!(f, r#"Web(id="{}" agent="{}")"#, id, agent),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct TunerUserPriority(i32);

impl TunerUserPriority {
    pub const MIN: i32 = -128;
    pub const MAX: i32 = 128;
    pub const GRAB: TunerUserPriority = TunerUserPriority(Self::MAX);

    pub fn is_grab(&self) -> bool {
        *self == Self::GRAB
    }
}

impl From<i32> for TunerUserPriority {
    fn from(value: i32) -> Self {
        TunerUserPriority(value.max(Self::MIN).min(Self::MAX))
    }
}

impl fmt::Display for TunerUserPriority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Clone)]
pub struct TunerUser {
    pub info: TunerUserInfo,
    pub priority: TunerUserPriority,
}

impl TunerUser {
    pub fn get_mirakurun_model(&self) -> MirakurunTunerUser {
        let (id, agent) = self.info.get_mirakurun_model();
        MirakurunTunerUser { id, agent, priority: self.priority.0 }
    }
}

impl fmt::Display for TunerUser {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (priority={})", self.info, self.priority)
    }
}

#[derive(Clone, Copy, Hash, Eq, PartialEq)]
#[derive(Deserialize, Serialize)]
pub struct TimeshiftRecordId(i64);

impl fmt::Display for TimeshiftRecordId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<i64> for TimeshiftRecordId {
    fn from(value: i64) -> Self {
        TimeshiftRecordId(value)
    }
}

// Mirakurun-compatible models

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[derive(Deserialize, Serialize)]
pub struct MirakurunServiceId(u64);

impl MirakurunServiceId {
    const MAGIC_NUMBER: u64 = 100_000;

    fn new(nid: NetworkId, sid: ServiceId) -> Self {
        // An unique identifier compatible with Mirakurun.
        // See src/Mirakurun/ServiceItem.ts in Chinachu/Mirakurun.
        MirakurunServiceId(
            nid.value() as u64 * Self::MAGIC_NUMBER + sid.value() as u64)
    }

    pub fn value(&self) -> u64 {
        self.0
    }

    pub fn nid(&self) -> NetworkId {
        NetworkId::from((self.0 / Self::MAGIC_NUMBER) as u16)
    }

    pub fn sid(&self) -> ServiceId {
        ServiceId::from((self.0 % Self::MAGIC_NUMBER) as u16)
    }
}

impl fmt::Display for MirakurunServiceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({} {})", self.0, self.nid(), self.sid())
    }
}

impl From<(NetworkId, ServiceId)> for MirakurunServiceId {
    fn from((nid, sid): (NetworkId, ServiceId)) -> Self {
        Self::new(nid, sid)
    }
}

impl From<ServiceTriple> for MirakurunServiceId {
    fn from(triple: ServiceTriple) -> Self {
        Self::new(triple.nid(), triple.sid())
    }
}

impl From<EventQuad> for MirakurunServiceId {
    fn from(quad: EventQuad) -> Self {
        Self::new(quad.nid(), quad.sid())
    }
}

impl From<MirakurunProgramId> for MirakurunServiceId {
    fn from(id: MirakurunProgramId) -> Self {
        Self::new(id.nid(), id.sid())
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[derive(Deserialize, Serialize)]
pub struct MirakurunProgramId(u64);

impl MirakurunProgramId {
    const MAGIC_NUMBER: u64 = 100_000;

    pub fn new(nid: NetworkId, sid: ServiceId, eid: EventId) -> Self {
        // An unique identifier compatible with Mirakurun.
        // See src/Mirakurun/ProgramItem.ts#L28 in Chinachu/Mirakurun.
        MirakurunProgramId(
            nid.value() as u64 * Self::MAGIC_NUMBER * Self::MAGIC_NUMBER
                + sid.value() as u64 * Self::MAGIC_NUMBER + eid.value() as u64)
    }

    pub fn nid(&self) -> NetworkId {
        NetworkId::from(
            (self.0 / (Self::MAGIC_NUMBER * Self::MAGIC_NUMBER)) as u16)
    }

    pub fn sid(&self) -> ServiceId {
        ServiceId::from(
            ((self.0 / Self::MAGIC_NUMBER) % Self::MAGIC_NUMBER) as u16)
    }

    pub fn eid(&self) -> EventId {
        EventId::from((self.0 % Self::MAGIC_NUMBER) as u16)
    }
}

impl fmt::Display for MirakurunProgramId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({} {} {})", self.0, self.nid(), self.sid(), self.eid())
    }
}

impl From<EventQuad> for MirakurunProgramId {
    fn from(quad: EventQuad) -> Self {
        MirakurunProgramId::new(quad.nid(), quad.sid(), quad.eid())
    }
}

#[derive(Debug)]
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MirakurunTuner {
    pub index: usize,
    pub name: String,
    #[serde(rename = "types")]
    pub channel_types: Vec<ChannelType>,
    pub command: Option<String>,
    pub pid: Option<u32>,
    pub users: Vec<MirakurunTunerUser>,
    pub is_available: bool,
    pub is_remote: bool,
    pub is_free: bool,
    pub is_using: bool,
    pub is_fault: bool,
}

#[derive(Debug)]
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MirakurunTunerUser {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    pub priority: i32,
    // url, disableDecoder, streamSetting and streamInfo properties are not supported.
}

#[derive(Debug)]
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MirakurunChannel {
    #[serde(rename = "type")]
    pub channel_type: ChannelType,
    pub channel: String,
    pub name: String,
    pub services: Vec<MirakurunChannelService>,
}

#[derive(Debug)]
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MirakurunChannelService {
    pub id: MirakurunServiceId,
    pub service_id: ServiceId,
    pub transport_stream_id: TransportStreamId,  // incompatible with Mirakurun
    pub network_id: NetworkId,
    pub name: String,
}

#[derive(Debug)]
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MirakurunService {
    pub id: MirakurunServiceId,
    pub service_id: ServiceId,
    pub transport_stream_id: TransportStreamId,  // incompatible with Mirakurun
    pub network_id: NetworkId,
    #[serde(rename = "type")]
    pub service_type: u16,
    #[serde(default)]
    pub logo_id: i16,
    #[serde(default)]
    pub remote_control_key_id: u16,
    pub name: String,
    pub channel: MirakurunServiceChannel,
    pub has_logo_data: bool,
}

impl From<EpgService> for MirakurunService {
    fn from(sv: EpgService) -> Self {
        Self {
            id: (sv.nid, sv.sid).into(),
            service_id: sv.sid,
            transport_stream_id: sv.tsid,
            network_id: sv.nid,
            service_type: sv.service_type,
            logo_id: sv.logo_id,
            remote_control_key_id: sv.remote_control_key_id,
            name: sv.name,
            channel: sv.channel.into(),
            has_logo_data: false,
        }
    }
}

#[derive(Debug)]
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MirakurunServiceChannel {
    #[serde(rename = "type")]
    pub channel_type: ChannelType,
    pub channel: String,
}

impl From<EpgChannel> for MirakurunServiceChannel {
    fn from(ch: EpgChannel) -> Self {
        Self {
            channel_type: ch.channel_type,
            channel: ch.channel,
        }
    }
}

#[derive(Clone, Debug)]
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MirakurunProgram {
    pub id: MirakurunProgramId,
    pub event_id: EventId,
    pub service_id: ServiceId,
    pub transport_stream_id: TransportStreamId,  // incompatible with Mirakurun
    pub network_id: NetworkId,
    #[serde(with = "serde_jst")]
    pub start_at: DateTime<Jst>,
    #[serde(with = "serde_duration_in_millis")]
    pub duration: Duration,
    pub is_free: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extended: Option<IndexMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video: Option<MirakurunProgramVideo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<MirakurunProgramAudio>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub genres: Option<Vec<EpgGenre>>,
}

impl From<EpgProgram> for MirakurunProgram {
    fn from(program: EpgProgram) -> Self {
        Self {
            id: program.quad.into(),
            event_id: program.quad.eid(),
            service_id: program.quad.sid(),
            transport_stream_id: program.quad.tsid(),
            network_id: program.quad.nid(),
            start_at: program.start_at,
            duration: program.duration,
            is_free: !program.scrambled,
            name: program.name,
            description: program.description,
            extended: program.extended,
            video: program.video.map(MirakurunProgramVideo::from),
            // Unkike Mirakurun, return properties of the main audio.
            // Mirakurun returns the last audio info in descriptors.
            // See src/Mirakurun/epg.ts#L373 in Chinachu/Mirakurun.
            audio: program.audios
                .values()
                .find(|audio| audio.main_component_flag)
                .cloned()
                .map(MirakurunProgramAudio::from),
            genres: program.genres,
        }
    }
}

#[derive(Clone, Debug)]
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MirakurunProgramVideo {
    #[serde(rename = "type")]
    video_type: Option<String>,
    resolution: Option<String>,
    stream_content: u8,
    component_type: u8,
}

impl MirakurunProgramVideo {
    fn new(stream_content: u8, component_type: u8) -> Self {
        Self {
            video_type: Self::get_video_type(stream_content),
            resolution: Self::get_resolution(component_type),
            stream_content,
            component_type,
        }
    }

    fn get_video_type(stream_content: u8) -> Option<String> {
        match stream_content {
            0x01 => Some("mpeg2".to_string()),
            0x05 => Some("h.264".to_string()),
            0x09 => Some("h.265".to_string()),
            _    => None,
        }
    }

    fn get_resolution(component_type: u8) -> Option<String> {
        match component_type {
            0x01..=0x04 => Some("480i".to_string()),
            0x83        => Some("4320p".to_string()),
            0x91..=0x94 => Some("2160p".to_string()),
            0xA1..=0xA4 => Some("480p".to_string()),
            0xB1..=0xB4 => Some("1080i".to_string()),
            0xC1..=0xC4 => Some("720p".to_string()),
            0xD1..=0xD4 => Some("240p".to_string()),
            0xE1..=0xE4 => Some("1080p".to_string()),
            0xF1..=0xF4 => Some("180p".to_string()),
            _           => None,
        }
    }
}

impl From<ComponentDescriptor> for MirakurunProgramVideo {
    fn from(video: ComponentDescriptor) -> Self {
        Self::new(video.stream_content, video.component_type)
    }
}

#[derive(Clone, Debug)]
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MirakurunProgramAudio {
    sampling_rate: i32,
    component_type: u8,
}

impl MirakurunProgramAudio {
    fn new(sampling_rate: u8, component_type: u8) -> Self {
        Self {
            sampling_rate: Self::get_sampling_rate(sampling_rate),
            component_type,
        }
    }

    fn get_sampling_rate(sampling_rate: u8) -> i32 {
        match sampling_rate {
            1 => 16_000,
            2 => 22_050,
            3 => 24_000,
            5 => 32_000,
            6 => 44_100,
            7 => 48_000,
            _ => -1,
        }
    }
}

impl From<AudioComponentDescriptor> for MirakurunProgramAudio {
    fn from(audio: AudioComponentDescriptor) -> Self {
        Self::new(audio.sampling_rate, audio.component_type)
    }
}

#[cfg(test)]
mod test_helper {
    use super::*;

    // Don't move the implementation outside the module.  That break the
    // type-safeness of integral identifiers like NetworkId.

    impl From<(u16, u16, u16)> for ServiceTriple {
        fn from(triple: (u16, u16, u16)) -> ServiceTriple {
            ServiceTriple::new(
                triple.0.into(), triple.1.into(), triple.2.into())
        }
    }

    impl From<(u16, u16, u16, u16)> for EventQuad {
        fn from(quad: (u16, u16, u16, u16)) -> EventQuad {
            EventQuad::new(
                quad.0.into(), quad.1.into(), quad.2.into(), quad.3.into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_channel_type() {
        assert!(serde_json::from_str::<ChannelType>(r#""WOWOW""#).is_err());

        assert_eq!(
            serde_json::from_str::<ChannelType>(r#""GR""#).unwrap(),
            ChannelType::GR);

        assert_eq!(
            serde_json::from_str::<Vec<ChannelType>>(
                r#"["GR", "BS", "CS", "SKY"]"#).unwrap(),
            vec![ChannelType::GR, ChannelType::BS,
                 ChannelType::CS, ChannelType::SKY]);
    }

    #[test]
    fn test_tuner_user_priority() {
        assert_eq!(TunerUserPriority::default(), 0.into());
        assert_eq!(TunerUserPriority::from(128), 128.into());
        assert_eq!(TunerUserPriority::from(129), 128.into());
        assert_eq!(TunerUserPriority::from(-128), (-128).into());
        assert_eq!(TunerUserPriority::from(-129), (-128).into());

        assert!(TunerUserPriority::from(128).is_grab());
    }

    #[test]
    fn test_mirakurun_service_id() {
        let nid = 1.into();
        let sid = 2.into();
        let id = MirakurunServiceId::new(nid, sid);
        assert_eq!(MirakurunServiceId(1_00002), id);
        assert_eq!(nid, id.nid());
        assert_eq!(sid, id.sid());
    }

    #[test]
    fn test_mirakurun_program_id() {
        let nid = 1.into();
        let sid = 2.into();
        let eid = 3.into();
        let id = MirakurunProgramId::new(nid, sid, eid);
        assert_eq!(MirakurunProgramId(1_00002_00003), id);
        assert_eq!(nid, id.nid());
        assert_eq!(sid, id.sid());
        assert_eq!(eid, id.eid());
    }
}
