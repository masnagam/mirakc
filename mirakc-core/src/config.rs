use std::collections::HashMap;
use std::collections::HashSet;
use std::fs::File;
use std::net::SocketAddr;
use std::net::ToSocketAddrs;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;

use indexmap::IndexMap;
use itertools::Itertools;
use serde::Deserialize;
use url::Url;

use crate::models::*;
use crate::tuner::TunerSubscriptionId;

pub fn load<P: AsRef<Path>>(config_path: P) -> Arc<Config> {
    let config_path = config_path.as_ref();

    let reader = File::open(config_path).unwrap_or_else(|err| {
        panic!("Failed to open {:?}: {}", config_path, err);
    });
    let mut config: Config = serde_yml::from_reader(reader).unwrap_or_else(|err| {
        panic!("Failed to parse {:?}: {}", config_path, err);
    });

    config = config.normalize();

    match std::env::var_os("MIRAKC_CONFIG_SKIP_VALIDATION") {
        Some(v) if v == "1" => tracing::warn!("Skip validation"),
        _ => config.validate(),
    }

    config.last_modified = std::fs::metadata(config_path)
        .map(|metadata| metadata.modified().ok())
        .ok()
        .flatten();
    tracing::info!(?config_path, "Loaded");
    Arc::new(config)
}

// Use the widely used map-style representation for enum types in YAML.
//
// `serde_yml` serializes enum types using YAML tags by default.
// This makes it impossible for other YAML libraries such as js-yaml to parse
// config.yml without a custom schema definition for config.yml.
//
// Specify `#[serde(with = "serde_yml::with::singleton_map_recursive")]` or
// `#[serde(with = "serde_yml::with::singleton_map")]` to every field
// containing contains enum types which should be represented with the map-style
// in YAML.

#[derive(Clone, Default, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, derive(Debug))]
pub struct Config {
    #[serde(skip)]
    pub last_modified: Option<SystemTime>,
    #[serde(default)]
    pub epg: EpgConfig,
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub channels: Vec<ChannelConfig>,
    #[serde(default)]
    pub tuners: Vec<TunerConfig>,
    #[serde(default)]
    pub filters: FiltersConfig,
    #[serde(default)]
    pub pre_filters: HashMap<String, FilterConfig>,
    #[serde(default)]
    pub post_filters: HashMap<String, PostFilterConfig>,
    #[serde(default)]
    pub jobs: JobsConfig,
    #[serde(default)]
    pub recording: RecordingConfig,
    #[serde(default)]
    pub timeshift: TimeshiftConfig,
    #[serde(default)]
    #[serde(with = "serde_yml::with::singleton_map_recursive")]
    pub onair_program_trackers: IndexMap<String, OnairProgramTrackerConfig>,
    #[serde(default)]
    pub resource: ResourceConfig,
}

impl Config {
    pub fn normalize(mut self) -> Self {
        self.channels = ChannelConfig::normalize(self.channels);
        self.jobs = JobsConfig::normalize(self.jobs);
        self
    }

    pub fn has_onair_program_trackers(&self) -> bool {
        !self.onair_program_trackers.is_empty()
    }

    fn validate(&self) {
        self.epg.validate();
        self.server.validate();
        self.channels
            .iter()
            .enumerate()
            .for_each(|(i, config)| config.validate(i));
        // The channels[].name property should be a unique, but some scripts generating a
        // channels.yml use the same name in multiple channels.
        //
        // Allow duplicate names in a practical point of view.
        //
        // assert_eq!(self.channels.len(),
        //            self.channels.iter()
        //            .map(|config| &config.name)
        //            .unique()
        //            .count(),
        //            "config.channels: `name` must be a unique");
        self.tuners
            .iter()
            .enumerate()
            .for_each(|(i, config)| config.validate(i));
        assert_eq!(
            self.tuners.len(),
            self.tuners
                .iter()
                .map(|config| &config.name)
                .unique()
                .count(),
            "config.tuners: `name` must be a unique"
        );
        self.filters.validate();
        self.pre_filters
            .iter()
            .for_each(|(name, config)| config.validate("pre-filters", name));
        self.post_filters
            .iter()
            .for_each(|(name, config)| config.validate(name));
        self.jobs.validate();
        self.recording.validate();
        self.timeshift.validate();
        self.onair_program_trackers
            .iter()
            .for_each(|(name, config)| config.validate(name));
        self.resource.validate();

        let mut dedicated: HashMap<String, TunerUserInfo> = HashMap::new();
        for (name, config) in self
            .onair_program_trackers
            .iter()
            .filter_map(|(name, config)| match config {
                OnairProgramTrackerConfig::Local(ref config) => Some((name, config)),
                _ => None,
            })
        {
            if let Some(ref user) = dedicated.get(&config.uses.tuner) {
                panic!("config.tuners[{}]: dedicated for {user}", config.uses.tuner);
            }
            if self
                .tuners
                .iter()
                .filter(|tuner| !tuner.disabled)
                .all(|tuner| tuner.name != config.uses.tuner)
            {
                panic!(
                    "config.onair-program-trackers[{name}]: uses undefined tuner[{}]",
                    config.uses.tuner
                );
            }
            dedicated.insert(
                config.uses.tuner.clone(),
                TunerUserInfo::OnairProgramTracker(name.clone()),
            );
        }
        for (name, config) in self.timeshift.recorders.iter() {
            if let Some(ref user) = dedicated.get(&config.uses.tuner) {
                panic!("config.tuners[{}]: dedicated for {user}", config.uses.tuner);
            }
            if !self.channels.iter().any(|channel| {
                channel.channel_type == config.uses.channel_type
                    && channel.channel == config.uses.channel
            }) {
                panic!(
                    "config.timeshift.recorders[{name}]: uses undefined channel {}/{}",
                    config.uses.channel_type, config.uses.channel
                );
            }
            if let Some(tuner) = self
                .tuners
                .iter()
                .filter(|tuner| !tuner.disabled)
                .find(|tuner| tuner.name == config.uses.tuner)
            {
                assert!(
                    tuner.channel_types.contains(&config.uses.channel_type),
                    "config.timeshift.recorders[{name}]: uses tuner[{}] which is not for {}",
                    config.uses.tuner,
                    config.uses.channel_type,
                );
            } else {
                panic!(
                    "timeshift-recorder[{name}] uses non-existent tuner[{}]",
                    config.uses.tuner
                );
            }
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
#[serde(deny_unknown_fields)]
pub struct EpgConfig {
    #[serde(default)]
    pub cache_dir: Option<PathBuf>,
}

impl EpgConfig {
    fn validate(&self) {
        if let Some(cache_dir) = self.cache_dir.as_ref() {
            assert!(
                cache_dir.is_dir(),
                "config.epg: `cache_dir` must be a path to an existing directory"
            );
        } else if !crate::timeshift::is_rebuild_mode() {
            tracing::warn!(config = "epg.cache-dir", "Not specified");
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    #[serde(default = "ServerConfig::default_addrs")]
    #[serde(with = "serde_yml::with::singleton_map_recursive")]
    pub addrs: Vec<ServerAddr>,
    #[serde(default = "ServerConfig::default_stream_max_chunks")]
    pub stream_max_chunks: usize,
    #[serde(default = "ServerConfig::default_stream_chunk_size")]
    pub stream_chunk_size: usize,
    #[serde(default = "ServerConfig::default_stream_time_limit")]
    pub stream_time_limit: u64,
    #[serde(default, with = "humantime_serde")]
    pub program_stream_max_start_delay: Option<Duration>,
    #[serde(default)]
    pub mounts: IndexMap<String, MountConfig>, // keeps the insertion order
    #[serde(default)]
    pub folder_view_template_path: Option<PathBuf>,
}

impl ServerConfig {
    pub(crate) fn http_addrs(&self) -> impl Iterator<Item = SocketAddr> + '_ {
        self.addrs
            .iter()
            .filter_map(|addr| match addr {
                ServerAddr::Http(addr) => addr.to_socket_addrs().ok(),
                _ => None,
            })
            .flatten()
    }

    pub(crate) fn uds_paths(&self) -> impl Iterator<Item = &Path> {
        self.addrs.iter().filter_map(|addr| match addr {
            ServerAddr::Unix(path) => Some(path.as_path()),
            _ => None,
        })
    }

    fn default_addrs() -> Vec<ServerAddr> {
        vec![Default::default()]
    }

    fn default_stream_max_chunks() -> usize {
        // TODO: The same value is defined in the broadcaster module.
        1000
    }

    fn default_stream_chunk_size() -> usize {
        // TODO: The same value is defined in the broadcaster module.
        4096 * 8
    }

    fn default_stream_time_limit() -> u64 {
        16 * 1000 // 16s
    }

    fn validate(&self) {
        const SERVER_STREAM_TIME_LIMIT_MIN: u64 = 15_000;

        assert!(
            self.stream_time_limit >= SERVER_STREAM_TIME_LIMIT_MIN,
            "config.server.stream-time-limit: must be larger than or equal to {}",
            SERVER_STREAM_TIME_LIMIT_MIN
        );

        if let Some(max_start_delay) = self.program_stream_max_start_delay {
            assert!(
                max_start_delay < Duration::from_secs(24 * 3600),
                "config.server.program-stream-max-start-delay: \
                 must not be less than 24h"
            );
        }

        self.addrs
            .iter()
            .enumerate()
            .for_each(|(i, addr)| addr.validate(i));
        self.mounts
            .iter()
            .for_each(|(mp, config)| config.validate(mp));

        if let Some(ref path) = self.folder_view_template_path {
            assert!(
                path.is_file(),
                "config.server.folder-view-template-path: must be a path to an existing file"
            );
        }
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            addrs: Self::default_addrs(),
            stream_max_chunks: Self::default_stream_max_chunks(),
            stream_chunk_size: Self::default_stream_chunk_size(),
            stream_time_limit: Self::default_stream_time_limit(),
            program_stream_max_start_delay: None,
            mounts: Default::default(),
            folder_view_template_path: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum ServerAddr {
    Http(String),
    Unix(PathBuf),
}

impl ServerAddr {
    fn validate(&self, i: usize) {
        match self {
            Self::Http(addr) => assert!(
                addr.to_socket_addrs().is_ok(),
                "config.server.addrs[{i}]: invalid socket address: {addr}"
            ),
            Self::Unix(_) => (),
        }
    }
}

impl Default for ServerAddr {
    fn default() -> Self {
        ServerAddr::Http("localhost:40772".to_string())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
#[serde(deny_unknown_fields)]
pub struct MountConfig {
    pub path: PathBuf,
    #[serde(default)]
    pub index: Option<PathBuf>,
    #[serde(default)]
    pub listing: bool,
}

impl MountConfig {
    fn validate(&self, mount_point: &str) {
        const MOUNT_POINT_BLOCK_LIST: [&str; 3] = ["/", "/api", "/events"];
        for blocked in MOUNT_POINT_BLOCK_LIST {
            assert!(
                mount_point != blocked,
                r#"config.server.mounts[{blocked}]: cannot mount onto "{blocked}""#,
            );
        }
        assert!(
            mount_point.starts_with('/'),
            "config.server.mounts[{}]: \
             a mount point must starts with '/'",
            mount_point
        );
        assert!(
            !mount_point.ends_with('/'),
            "config.server.mounts[{}]: \
             a mount point must not ends with '/'",
            mount_point
        );
        assert!(
            self.path.exists(),
            "config.server.mounts[{}].path: \
             must be a path to an existing entry",
            mount_point
        );
        if let Some(index) = self.index.as_ref() {
            let path = self.path.join(index);
            assert!(
                path.is_file(),
                "config.server.mounts[{}].index: \
                 must be an existing file if it exists",
                mount_point
            );
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[serde(deny_unknown_fields)]
pub struct ChannelConfig {
    pub name: String,
    #[serde(rename = "type")]
    pub channel_type: ChannelType,
    pub channel: String,
    #[serde(default)]
    pub extra_args: String,
    #[serde(default)]
    pub services: Vec<Sid>,
    #[serde(default)]
    pub excluded_services: Vec<Sid>,
    #[serde(default)]
    pub disabled: bool,
}

impl ChannelConfig {
    fn normalize(channels: Vec<ChannelConfig>) -> Vec<ChannelConfig> {
        let mut normalized: Vec<ChannelConfig> = vec![];
        // Remove disabled channels and merge channels having the same `type`
        // and `channel`.
        for mut channel in channels.into_iter().filter(|ch| !ch.disabled) {
            match normalized.iter_mut().find(|ch| **ch == channel) {
                Some(ch) => {
                    if ch.extra_args != channel.extra_args {
                        tracing::warn!(
                            "Channels having the same `type` and `channel` \
                             should have the same `extra-args`"
                        );
                    }
                    if ch.services.is_empty() {
                        // Collect all services.
                    } else if channel.services.is_empty() {
                        // Collect all services.
                        ch.services = vec![];
                    } else {
                        ch.services.append(&mut channel.services);
                    }
                    ch.excluded_services.append(&mut channel.excluded_services);
                }
                None => normalized.push(channel),
            }
        }
        // Normalize
        for channel in normalized.iter_mut() {
            channel.services = channel.services.iter().sorted().unique().cloned().collect();
            channel.excluded_services = channel
                .excluded_services
                .iter()
                .sorted()
                .unique()
                .cloned()
                .collect();
        }
        normalized
    }

    fn validate(&self, index: usize) {
        debug_assert!(!self.disabled);
        assert!(
            !self.name.is_empty(),
            "config.channels[{}].name: must be a non-empty string",
            index
        );
        assert!(
            !self.channel.is_empty(),
            "config.channels[{}].channel: must be a non-empty string",
            index
        );
    }
}

impl PartialEq for ChannelConfig {
    fn eq(&self, other: &Self) -> bool {
        self.channel_type == other.channel_type && self.channel == other.channel
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
#[serde(deny_unknown_fields)]
pub struct TunerConfig {
    pub name: String,
    #[serde(rename = "types")]
    pub channel_types: Vec<ChannelType>,
    pub command: String,
    #[serde(default = "TunerConfig::default_time_limit")]
    pub time_limit: u64,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub decoded: bool,
}

impl TunerConfig {
    fn default_time_limit() -> u64 {
        30 * 1000 // 30s
    }

    fn validate(&self, index: usize) {
        if self.disabled {
            return;
        }
        assert!(
            !self.name.is_empty(),
            "config.tuners[{}].name: must be a non-empty string",
            index
        );
        assert!(
            !self.channel_types.is_empty(),
            "config.tuners[{}].types: must be a non-empty list",
            index
        );
        assert!(
            !self.command.is_empty(),
            "config.tuners[{}].command: must be a non-empty string",
            index
        );
    }
}

impl Default for TunerConfig {
    fn default() -> Self {
        TunerConfig {
            name: "".to_string(),
            channel_types: vec![],
            command: "".to_string(),
            time_limit: Self::default_time_limit(),
            disabled: false,
            decoded: false,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
#[serde(deny_unknown_fields)]
pub struct FiltersConfig {
    #[serde(default)]
    pub tuner_filter: FilterConfig,
    #[serde(default = "FiltersConfig::default_service_filter")]
    pub service_filter: FilterConfig,
    #[serde(default = "FiltersConfig::default_program_filter")]
    pub program_filter: FilterConfig,
    #[serde(default)]
    pub decode_filter: FilterConfig,
}

impl FiltersConfig {
    fn default_service_filter() -> FilterConfig {
        FilterConfig {
            command: "mirakc-arib filter-service --sid={{{sid}}}".to_string(),
        }
    }

    fn default_program_filter() -> FilterConfig {
        // Historical Notes
        // ----------------
        // The --pre-streaming option is NOT used anymore due to the issue#30.
        // Without the --pre-streaming option, actix-web cannot detect the
        // client disconnection until trying to write data to the socket.  In
        // the worst case, the client disconnect is not detected for a few
        // seconds which depends on a transmission interval of EIT p/f packets.
        //
        // The --pre-streaming was added in order to avoid the issue#1313 in
        // actix/actix-web.  PAT TS packets will be sent before the program
        // starts when this option is specified.  See masnagam/rust-case-studies
        // for details about the issue#1313.
        FilterConfig {
            command: "mirakc-arib filter-program --sid={{{sid}}} --eid={{{eid}}} \
                      --clock-pid={{{clock_pid}}} --clock-pcr={{{clock_pcr}}} \
                      --clock-time={{{clock_time}}} --end-margin=2000\
                      {{#video_tags}} --video-tag={{{.}}}{{/video_tags}}\
                      {{#audio_tags}} --audio-tag={{{.}}}{{/audio_tags}}\
                      {{#wait_until}} --wait-until={{{.}}}{{/wait_until}}\
                      "
            .to_string(),
        }
    }

    fn validate(&self) {
        // self.tuner_filter.command may be an empty string.
        self.service_filter.validate("filters", "service-filter");
        self.program_filter.validate("filters", "program-filter");
        // self.decode_filter.command may be an empty string.
    }
}

impl Default for FiltersConfig {
    fn default() -> Self {
        FiltersConfig {
            tuner_filter: Default::default(),
            service_filter: Self::default_service_filter(),
            decode_filter: Default::default(),
            program_filter: Self::default_program_filter(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
#[serde(deny_unknown_fields)]
pub struct FilterConfig {
    #[serde(default)]
    pub command: String,
}

impl FilterConfig {
    fn validate(&self, group: &str, name: &str) {
        assert!(
            !self.command.is_empty(),
            "config.{}[{}].command: must be a non-empty string",
            group,
            name
        );
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
#[serde(deny_unknown_fields)]
pub struct PostFilterConfig {
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub content_type: Option<String>,
}

impl PostFilterConfig {
    fn validate(&self, name: &str) {
        assert!(
            !self.command.is_empty(),
            "config.post-filters[{}].command: must be a non-empty string",
            name
        );
        if let Some(content_type) = self.content_type.as_ref() {
            assert!(
                !content_type.is_empty(),
                "config.post-filters[{}].content-type: must be a non-empty string",
                name
            );
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
#[serde(deny_unknown_fields)]
pub struct JobsConfig {
    #[serde(default = "JobsConfig::default_scan_services")]
    pub scan_services: JobConfig,
    #[serde(default = "JobsConfig::default_sync_clocks")]
    pub sync_clocks: JobConfig,
    #[serde(default = "JobsConfig::default_update_schedules")]
    pub update_schedules: JobConfig,
}

impl JobsConfig {
    fn default_scan_services() -> JobConfig {
        JobConfig {
            command: "mirakc-arib scan-services\
                      {{#sids}} --sids={{{.}}}{{/sids}}\
                      {{#xsids}} --xsids={{{.}}}{{/xsids}}"
                .to_string(),
            schedule: "0 1 8,20 * * * *".to_string(),
            disabled: false,
        }
    }

    fn default_sync_clocks() -> JobConfig {
        JobConfig {
            command: "mirakc-arib sync-clocks\
                      {{#sids}} --sids={{{.}}}{{/sids}}\
                      {{#xsids}} --xsids={{{.}}}{{/xsids}}"
                .to_string(),
            schedule: "0 11 8,20 * * * *".to_string(),
            disabled: false,
        }
    }

    fn default_update_schedules() -> JobConfig {
        JobConfig {
            command: "mirakc-arib collect-eits\
                      {{#sids}} --sids={{{.}}}{{/sids}}\
                      {{#xsids}} --xsids={{{.}}}{{/xsids}}"
                .to_string(),
            schedule: "0 21 8,20 * * * *".to_string(),
            disabled: false,
        }
    }

    fn normalize(mut self) -> Self {
        if !self.scan_services.disabled {
            if self.scan_services.command.is_empty() {
                self.scan_services.command = Self::default_scan_services().command;
            }
            if self.scan_services.schedule.is_empty() {
                self.scan_services.schedule = Self::default_scan_services().schedule;
            }
        }
        if !self.sync_clocks.disabled {
            if self.sync_clocks.command.is_empty() {
                self.sync_clocks.command = Self::default_sync_clocks().command;
            }
            if self.sync_clocks.schedule.is_empty() {
                self.sync_clocks.schedule = Self::default_sync_clocks().schedule;
            }
        }
        if !self.update_schedules.disabled {
            if self.update_schedules.command.is_empty() {
                self.update_schedules.command = Self::default_update_schedules().command;
            }
            if self.update_schedules.schedule.is_empty() {
                self.update_schedules.schedule = Self::default_update_schedules().schedule;
            }
        }
        self
    }

    fn validate(&self) {
        self.scan_services.validate("scan-services");
        self.sync_clocks.validate("sync-clocks");
        self.update_schedules.validate("update-schedules");
    }
}

impl Default for JobsConfig {
    fn default() -> Self {
        JobsConfig {
            scan_services: Self::default_scan_services(),
            sync_clocks: Self::default_sync_clocks(),
            update_schedules: Self::default_update_schedules(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
#[serde(deny_unknown_fields)]
pub struct JobConfig {
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub schedule: String,
    #[serde(default)]
    pub disabled: bool,
}

impl JobConfig {
    fn validate(&self, name: &str) {
        if self.disabled {
            if !crate::timeshift::is_rebuild_mode() {
                tracing::warn!(config = format!("jobs.{}", name), "Disabled");
            }
        } else {
            assert!(
                !self.command.is_empty(),
                "config.jobs[{}].command: must be a non-empty string",
                name
            );
            assert!(
                cron::Schedule::from_str(&self.schedule).is_ok(),
                "config.jobs[{}].schedule: not valid",
                name
            );
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
#[serde(deny_unknown_fields)]
pub struct RecordingConfig {
    pub basedir: Option<PathBuf>,
}

impl RecordingConfig {
    pub fn is_enabled(&self) -> bool {
        self.basedir.is_some()
    }

    fn validate(&self) {
        if let Some(ref basedir) = self.basedir {
            assert!(
                basedir.is_absolute(),
                "config.recording.basedir: must be an absolute path"
            );
            assert!(
                basedir.is_dir(),
                "config.recording.basedir: must be a path to an existing directory"
            );
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
#[serde(deny_unknown_fields)]
pub struct TimeshiftConfig {
    #[serde(default = "TimeshiftConfig::default_command")]
    pub command: String,
    #[serde(default)]
    pub recorders: IndexMap<String, TimeshiftRecorderConfig>,
}

impl TimeshiftConfig {
    pub fn is_enabled(&self) -> bool {
        !self.command.is_empty() && !self.recorders.is_empty()
    }

    fn default_command() -> String {
        "mirakc-arib record-service --sid={{{sid}}} --file={{{file}}} \
         --chunk-size={{{chunk_size}}} --num-chunks={{{num_chunks}}} \
         --start-pos={{{start_pos}}}"
            .to_string()
    }

    fn validate(&self) {
        assert!(
            !self.command.is_empty(),
            "config.timeshift.command: must be a non-empty string"
        );
        self.recorders
            .iter()
            .for_each(|(name, config)| config.validate(name));
    }
}

impl Default for TimeshiftConfig {
    fn default() -> Self {
        TimeshiftConfig {
            command: TimeshiftConfig::default_command(),
            recorders: IndexMap::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
#[serde(deny_unknown_fields)]
pub struct TimeshiftRecorderConfig {
    pub service_id: ServiceId,
    pub ts_file: PathBuf,
    pub data_file: PathBuf,
    #[serde(default = "TimeshiftRecorderConfig::default_chunk_size")]
    pub chunk_size: usize,
    pub num_chunks: usize,
    #[serde(default = "TimeshiftRecorderConfig::default_num_reserves")]
    pub num_reserves: usize,
    #[serde(default = "TimeshiftRecorderConfig::default_priority")]
    pub priority: i32,
    pub uses: TimeshiftRecorderUses,
}

impl TimeshiftRecorderConfig {
    const BUFSIZE: usize = 8192;
    const TS_PACKET_SIZE: usize = 188;

    pub fn max_file_size(&self) -> u64 {
        (self.chunk_size as u64) * (self.num_chunks as u64)
    }

    pub fn max_chunks(&self) -> usize {
        assert!(self.num_chunks > self.num_reserves);
        self.num_chunks - self.num_reserves
    }

    fn validate(&self, name: &str) {
        assert!(
            self.ts_file.is_absolute(),
            "config.timeshift.recorders[{name}]: \
             `ts-file` must be an absolute path"
        );
        assert!(
            self.ts_file.to_str().is_some(),
            "config.timeshift.recorders[{name}]: \
             `ts-file` path must consist only of UTF-8 compatible characters"
        );
        assert!(
            self.ts_file.is_file(),
            "config.timeshift.recorders[{name}]: \
             `ts-file` must be a path to a regular file"
        );
        assert!(
            self.data_file.is_absolute(),
            "config.timeshift.recorders[{name}]: \
             `data-file` must be an absolute path"
        );
        assert!(
            self.data_file.to_str().is_some(),
            "config.timeshift.recorders[{name}]: \
             `data-file` path must consist only of UTF-8 compatible characters"
        );
        assert!(
            self.data_file.is_file(),
            "config.timeshift.recorders[{name}]: \
             `data-file` must be a path to a regular file"
        );
        // TODO
        // ----
        // We should save data in a binary format in a performance point of view.  However, the
        // following crates don't work properly (maybe, due to bugs...).
        //
        //   * bincode
        //   * rmp_serde
        //
        // We may support a binary format in the future if there is a crate that works well with
        // our data formats.
        assert!(
            self.data_file.extension().is_some() && self.data_file.extension().unwrap() == "json",
            "config.timeshift.recorders[{name}]: \
             `data-file` must be a JSON file"
        );
        assert!(
            self.chunk_size > 0,
            "config.timeshift.recorders[{name}]: \
             `chunk-size` must be larger than 0"
        );
        assert!(
            self.chunk_size % (Self::BUFSIZE) == 0,
            "config.timeshift.recorders[{name}]: \
             `chunk-size` must be a multiple of {}",
            Self::BUFSIZE,
        );
        assert!(
            self.num_chunks > 2,
            "config.timeshift.recorders[{name}]: \
             `num-chunks` must be larger than 2"
        );
        assert!(
            self.num_reserves > 0,
            "config.timeshift.recorders[{name}]: \
             `num-reserves` must be larger than 0"
        );
        assert!(
            self.num_chunks - self.num_reserves > 1,
            "config.timeshift.recorders[{name}]: \
             Maximum number of available chunks \
             (`num-chunks` - `num-reserves`) must be larger than 1"
        );

        self.uses.validate(name);

        let ts_file_size = match self.ts_file.metadata() {
            Ok(metadata) => metadata.len(),
            Err(err) => unreachable!(
                "config.timeshift.recorders[{name}]: \
                 Failed to get the size of `ts-file`: {err}"
            ),
        };
        assert_eq!(
            self.max_file_size(),
            ts_file_size,
            "config.timeshift.recorders[{name}]: \
             `ts-file` must be allocated with {} in advance",
            self.max_file_size()
        );
    }

    fn default_chunk_size() -> usize {
        Self::BUFSIZE * Self::TS_PACKET_SIZE * 100
    }

    fn default_num_reserves() -> usize {
        1
    }

    fn default_priority() -> i32 {
        TunerUserPriority::MAX
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
#[serde(deny_unknown_fields)]
pub struct TimeshiftRecorderUses {
    pub tuner: String, // config.tuners[].name
    pub channel_type: ChannelType,
    pub channel: String,
}

impl TimeshiftRecorderUses {
    fn validate(&self, name: &str) {
        assert!(
            !self.tuner.is_empty(),
            "config.timeshift.recorders[{name}].uses.tuner: \
             must be a non-empty string"
        );
        assert!(
            !self.channel.is_empty(),
            "config.timeshift.recorders[{name}].uses.channel: \
             must be a non-empty string"
        );
    }
}

#[derive(Clone, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, derive(Debug))]
pub enum OnairProgramTrackerConfig {
    Local(Arc<LocalOnairProgramTrackerConfig>),
    Remote(Arc<RemoteOnairProgramTrackerConfig>),
}

impl OnairProgramTrackerConfig {
    fn validate(&self, name: &str) {
        match self {
            Self::Local(config) => config.validate(name),
            Self::Remote(config) => config.validate(name),
        }
    }
}

#[derive(Clone, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, derive(Debug))]
pub struct LocalOnairProgramTrackerConfig {
    pub channel_types: HashSet<ChannelType>,
    #[serde(default)]
    pub services: HashSet<ServiceId>,
    #[serde(default)]
    pub excluded_services: HashSet<ServiceId>,
    #[serde(default = "LocalOnairProgramTrackerConfig::default_command")]
    pub command: String,
    pub uses: LocalOnairProgramTrackerUses,
    // Internal properties
    #[serde(default, skip)]
    pub stream_id: Option<TunerSubscriptionId>,
}

impl LocalOnairProgramTrackerConfig {
    pub fn default_command() -> String {
        "timeout 5s mirakc-arib collect-eitpf --sids={{{sid}}}".to_string()
    }

    fn validate(&self, name: &str) {
        assert!(
            !self.channel_types.is_empty(),
            "config.onair-program-trackers[{name}].channel-types: \
             must be a non-empty list"
        );
        assert!(
            !self.command.is_empty(),
            "config.onair-program-trackers[{name}]command: \
             must be a non-empty string"
        );
        self.uses.validate(name);
    }
}

#[derive(Clone, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, derive(Debug))]
pub struct LocalOnairProgramTrackerUses {
    pub tuner: String, // config.tuners[].name
}

impl LocalOnairProgramTrackerUses {
    fn validate(&self, name: &str) {
        assert!(
            !self.tuner.is_empty(),
            "config.onair-program-trackers[{name}].uses.tuner: \
             must be a non-empty string"
        );
    }
}

#[derive(Clone, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, derive(Debug))]
pub struct RemoteOnairProgramTrackerConfig {
    #[serde(with = "serde_yml::with::singleton_map")]
    pub url: Url,
    #[serde(default)]
    pub services: HashSet<ServiceId>,
    #[serde(default)]
    pub excluded_services: HashSet<ServiceId>,
    #[serde(default = "RemoteOnairProgramTrackerConfig::default_events_endpoint")]
    pub events_endpoint: String,
    #[serde(default = "RemoteOnairProgramTrackerConfig::default_onair_endpoint")]
    pub onair_endpoint: String,
}

impl RemoteOnairProgramTrackerConfig {
    pub fn events_url(&self) -> Url {
        self.url.join(&self.events_endpoint).unwrap()
    }

    pub fn onair_url(&self) -> Url {
        self.url.join(&self.onair_endpoint).unwrap()
    }

    pub fn onair_url_of(&self, service_id: ServiceId) -> Url {
        self.onair_url()
            .join(&format!("{}/{}", self.onair_endpoint, service_id.value()))
            .unwrap()
    }

    fn default_events_endpoint() -> String {
        "/events".to_string()
    }

    fn default_onair_endpoint() -> String {
        "/api/onair".to_string()
    }

    fn validate(&self, _name: &str) {}
}

impl Default for RemoteOnairProgramTrackerConfig {
    fn default() -> Self {
        RemoteOnairProgramTrackerConfig {
            url: Url::parse("http://localhost:40772/").unwrap(),
            services: Default::default(),
            excluded_services: Default::default(),
            events_endpoint: Self::default_events_endpoint(),
            onair_endpoint: Self::default_onair_endpoint(),
        }
    }
}

#[derive(Clone, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, derive(Debug))]
pub enum RemoteOnairProgramTrackerUrl {
    Mirakc(Url),
}

#[derive(Clone, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, derive(Debug))]
pub struct ResourceConfig {
    #[serde(default = "ResourceConfig::default_strings_yaml")]
    pub strings_yaml: String,
    #[serde(default)]
    #[serde(deserialize_with = "ResourceConfig::deserialize_logos")]
    pub logos: HashMap<ServiceId, String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
#[serde(deny_unknown_fields)]
pub struct LogoConfig {
    pub service_id: ServiceId,
    pub image: String,
}

impl ResourceConfig {
    fn default_strings_yaml() -> String {
        "/etc/mirakc/strings.yml".to_string()
    }

    fn validate(&self) {
        assert!(
            Path::new(&self.strings_yaml).is_file(),
            "config.resources.strings-yaml: must be a path to an existing YAML file"
        );
        for (service_id, image) in self.logos.iter() {
            assert!(
                Path::new(image).is_file(),
                "config.resources.logos[{}]: must be a path to an existing file",
                service_id
            );
        }
    }

    fn deserialize_logos<'de, D>(deserializer: D) -> Result<HashMap<ServiceId, String>, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let values: Vec<LogoConfig> = Vec::deserialize(deserializer)?;
        let mut logos = HashMap::new();
        for logo in values.into_iter() {
            logos.insert(logo.service_id, logo.image);
        }
        Ok(logos)
    }
}

impl Default for ResourceConfig {
    fn default() -> Self {
        ResourceConfig {
            strings_yaml: Self::default_strings_yaml(),
            logos: Default::default(),
        }
    }
}

#[allow(clippy::field_reassign_with_default)]
#[allow(clippy::needless_update)]
#[cfg(test)]
mod tests {
    #[cfg(not(target_os = "macos"))]
    use std::os::fd::AsRawFd;

    use super::*;
    use indexmap::indexmap;
    use maplit::hashmap;
    use maplit::hashset;
    use tempfile::NamedTempFile;
    use test_log::test;

    #[test]
    fn test_config() {
        assert_eq!(
            serde_yml::from_str::<Config>("{}").unwrap(),
            Default::default()
        );

        let result = serde_yml::from_str::<Config>(
            r#"
            unknown:
              property: value
        "#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_config_has_onair_program_trackers() {
        assert!(!Config::default().has_onair_program_trackers());

        let config = serde_yml::from_str::<Config>(
            r#"
            onair-program-trackers:
              test:
                remote:
                  url: http://test
        "#,
        )
        .unwrap();
        assert!(config.has_onair_program_trackers());
    }

    #[test]
    fn test_config_validate() {
        let mut config = Config::default();
        config.resource.strings_yaml = "/bin/sh".to_string();
        config.validate();
    }

    #[test]
    fn test_config_validate_channel_names() {
        let config = serde_yml::from_str::<Config>(
            r#"
            channels:
              - name: test
                type: GR
                channel: test
              - name: test
                type: GR
                channel: test1
            resource:
              strings-yaml: /bin/sh
        "#,
        )
        .unwrap();
        config.validate();
    }

    #[test]
    #[should_panic(expected = "config.tuners: `name` must be a unique")]
    fn test_config_validate_tuner_names() {
        let config = serde_yml::from_str::<Config>(
            r#"
            tuners:
              - name: test
                types: [GR]
                command: test
              - name: test
                types: [GR]
                command: test
            resource:
              strings-yaml: /bin/sh
        "#,
        )
        .unwrap();
        config.validate();
    }

    #[test]
    #[should_panic(
        expected = "config.onair-program-trackers[tracker]: uses undefined tuner[tuner]"
    )]
    fn test_config_validate_tracker_uses_undefined_tuner() {
        let config = serde_yml::from_str::<Config>(
            r#"
            tuners:
              - name: test
                types: [GR]
                command: test
            onair-program-trackers:
              tracker:
                local:
                  channel-types: [GR]
                  uses:
                    tuner: tuner
            resource:
              strings-yaml: /bin/sh
        "#,
        )
        .unwrap();
        config.validate();
    }

    #[test]
    #[should_panic(expected = "config.tuners[test]: dedicated for OnairProgramTracker(tracker1)")]
    fn test_config_validate_tuner_usage_conflict() {
        let config = serde_yml::from_str::<Config>(
            r#"
            tuners:
              - name: test
                types: [GR,BS]
                command: test
            onair-program-trackers:
              tracker1:
                local:
                  channel-types: [GR]
                  uses:
                    tuner: test
              tracker2:
                local:
                  channel-types: [BS]
                  uses:
                    tuner: test
            resource:
              strings-yaml: /bin/sh
        "#,
        )
        .unwrap();
        config.validate();
    }

    #[test]
    fn test_config_validate_tuner_dedicated_for_onair_tracker() {
        let config = serde_yml::from_str::<Config>(
            r#"
            tuners:
              - name: test
                types: [GR]
                command: test
            onair-program-trackers:
              tracker:
                local:
                  channel-types: [GR]
                  uses:
                    tuner: test
            resource:
              strings-yaml: /bin/sh
        "#,
        )
        .unwrap();
        config.validate();
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn test_config_validate_tuner_dedicated_for_timeshift() {
        let chunk_size = TimeshiftRecorderConfig::BUFSIZE;
        let num_chunks = 3;
        let ts_file = NamedTempFile::new().unwrap();
        let ts_file_size = (chunk_size * num_chunks) as libc::off64_t;
        unsafe {
            let _ = libc::fallocate64(ts_file.as_raw_fd(), 0, 0, ts_file_size);
        }
        let data_file = tempfile::Builder::new().suffix(".json").tempfile().unwrap();
        let config = serde_yml::from_str::<Config>(&format!(
            r#"
            channels:
              - name: test
                type: GR
                channel: '26'
            tuners:
              - name: test
                types: [GR]
                command: test
            timeshift:
              recorders:
                recorder:
                  service-id: 3273701032
                  ts-file: {}
                  data-file: {}
                  chunk-size: {chunk_size}
                  num-chunks: {num_chunks}
                  uses:
                    tuner: test
                    channel-type: GR
                    channel: '26'
            resource:
              strings-yaml: /bin/sh
        "#,
            ts_file.path().to_str().unwrap(),
            data_file.path().to_str().unwrap(),
        ))
        .unwrap();
        config.validate();
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    #[should_panic(expected = "config.timeshift.recorders[recorder]: uses undefined channel GR/26")]
    fn test_config_validate_tuner_dedicated_for_timeshift_undefined_channel() {
        let chunk_size = TimeshiftRecorderConfig::BUFSIZE;
        let num_chunks = 3;
        let ts_file = NamedTempFile::new().unwrap();
        let ts_file_size = (chunk_size * num_chunks) as libc::off64_t;
        unsafe {
            let _ = libc::fallocate64(ts_file.as_raw_fd(), 0, 0, ts_file_size);
        }
        let data_file = tempfile::Builder::new().suffix(".json").tempfile().unwrap();
        let config = serde_yml::from_str::<Config>(&format!(
            r#"
            channels:
              - name: test
                type: GR
                channel: '27'
            tuners:
              - name: test
                types: [GR]
                command: test
            timeshift:
              recorders:
                recorder:
                  service-id: 3273701032
                  ts-file: {}
                  data-file: {}
                  chunk-size: {chunk_size}
                  num-chunks: {num_chunks}
                  uses:
                    tuner: test
                    channel-type: GR
                    channel: '26'
            resource:
              strings-yaml: /bin/sh
        "#,
            ts_file.path().to_str().unwrap(),
            data_file.path().to_str().unwrap()
        ))
        .unwrap();
        config.validate();
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    #[should_panic(
        expected = "config.timeshift.recorders[recorder]: uses tuner[test] which is not for GR"
    )]
    fn test_config_validate_tuner_dedicated_for_timeshift_unsupported_channel_type() {
        let chunk_size = TimeshiftRecorderConfig::BUFSIZE;
        let num_chunks = 3;
        let ts_file = NamedTempFile::new().unwrap();
        let ts_file_size = (chunk_size * num_chunks) as libc::off64_t;
        unsafe {
            let _ = libc::fallocate64(ts_file.as_raw_fd(), 0, 0, ts_file_size);
        }
        let data_file = tempfile::Builder::new().suffix(".json").tempfile().unwrap();
        let config = serde_yml::from_str::<Config>(&format!(
            r#"
            channels:
              - name: test
                type: GR
                channel: '26'
            tuners:
              - name: test
                types: [BS]
                command: test
            timeshift:
              recorders:
                recorder:
                  service-id: 3273701032
                  ts-file: {}
                  data-file: {}
                  chunk-size: {chunk_size}
                  num-chunks: {num_chunks}
                  uses:
                    tuner: test
                    channel-type: GR
                    channel: '26'
            resource:
              strings-yaml: /bin/sh
        "#,
            ts_file.path().to_str().unwrap(),
            data_file.path().to_str().unwrap(),
        ))
        .unwrap();
        config.validate();
    }

    #[test]
    fn test_epg_config() {
        assert_eq!(
            serde_yml::from_str::<EpgConfig>("{}").unwrap(),
            Default::default()
        );

        assert_eq!(
            serde_yml::from_str::<EpgConfig>(
                r#"
                cache-dir: /path/to/epg
            "#
            )
            .unwrap(),
            EpgConfig {
                cache_dir: Some("/path/to/epg".into()),
            }
        );

        let result = serde_yml::from_str::<EpgConfig>(
            r#"
            unknown:
              property: value
        "#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_epg_config_validate() {
        let config = EpgConfig::default();
        config.validate();
    }

    #[test]
    #[should_panic(expected = "config.epg: `cache_dir` must be a path to an existing directory")]
    fn test_epg_config_validate_cache_dir() {
        let mut config = EpgConfig::default();
        config.cache_dir = Some("/path/to/dir".into());
        config.validate();
    }

    #[test]
    fn test_server_config() {
        assert_eq!(
            serde_yml::from_str::<ServerConfig>("{}").unwrap(),
            Default::default()
        );

        let mut config = ServerConfig::default();
        config.addrs = vec![ServerAddr::Http("0.0.0.0:40772".to_string())];
        assert_eq!(
            serde_yml::from_str::<ServerConfig>(
                r#"
                addrs:
                  - http: '0.0.0.0:40772'
            "#
            )
            .unwrap(),
            config
        );

        let mut config = ServerConfig::default();
        config.addrs = vec![ServerAddr::Unix("/path/to/sock".into())];
        assert_eq!(
            serde_yml::from_str::<ServerConfig>(
                r#"
                addrs:
                  - unix: /path/to/sock
            "#
            )
            .unwrap(),
            config
        );

        let mut config = ServerConfig::default();
        config.addrs = vec![
            ServerAddr::Http("0.0.0.0:40772".to_string()),
            ServerAddr::Unix("/path/to/sock".into()),
        ];
        assert_eq!(
            serde_yml::from_str::<ServerConfig>(
                r#"
                addrs:
                  - http: '0.0.0.0:40772'
                  - unix: /path/to/sock
            "#
            )
            .unwrap(),
            config
        );

        let mut config = ServerConfig::default();
        config.stream_max_chunks = 1000;
        assert_eq!(
            serde_yml::from_str::<ServerConfig>(
                r#"
                stream-max-chunks: 1000
            "#
            )
            .unwrap(),
            config
        );

        let mut config = ServerConfig::default();
        config.stream_chunk_size = 10000;
        assert_eq!(
            serde_yml::from_str::<ServerConfig>(
                r#"
                stream-chunk-size: 10000
            "#
            )
            .unwrap(),
            config
        );

        let mut config = ServerConfig::default();
        config.stream_time_limit = 10000;
        assert_eq!(
            serde_yml::from_str::<ServerConfig>(
                r#"
                stream-time-limit: 10000
            "#
            )
            .unwrap(),
            config
        );

        let mut config = ServerConfig::default();
        config.program_stream_max_start_delay = Some(Duration::from_secs(3600));
        assert_eq!(
            serde_yml::from_str::<ServerConfig>(
                r#"
                program-stream-max-start-delay: 1h
            "#
            )
            .unwrap(),
            config
        );

        let mut config = ServerConfig::default();
        config.mounts = indexmap! {
            "/ui".to_string() => MountConfig {
                path: "/path/to/ui".into(),
                index: None,
                listing: false,
            },
            "/public".to_string() => MountConfig {
                path: "/path/to/public".into(),
                index: None,
                listing: true,
            },
            "/".to_string() => MountConfig {
                path: "/path/to/folder".into(),
                index: Some("index.html".into()),
                listing: false,
            },
        };
        assert_eq!(
            serde_yml::from_str::<ServerConfig>(
                r#"
                mounts:
                  /ui:
                    path: /path/to/ui
                  /public:
                    path: /path/to/public
                    listing: true
                  /:
                    path: /path/to/folder
                    index: index.html
            "#
            )
            .unwrap(),
            config
        );

        let mut config = ServerConfig::default();
        config.folder_view_template_path = Some("/path/to/listing.html.mustache".into());
        assert_eq!(
            serde_yml::from_str::<ServerConfig>(
                r#"
                folder-view-template-path: /path/to/listing.html.mustache
            "#
            )
            .unwrap(),
            config
        );

        let result = serde_yml::from_str::<ServerConfig>(
            r#"
            unknown:
              property: value
        "#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_server_config_validate() {
        let config = ServerConfig::default();
        config.validate();
    }

    #[test]
    #[should_panic(
        expected = "config.server.stream-time-limit: must be larger than or equal to 15000"
    )]
    fn test_server_config_validate_stream_time_limit() {
        let mut config = ServerConfig::default();
        config.stream_time_limit = 1;
        config.validate();
    }

    #[test]
    fn test_server_config_validate_max_start_delay() {
        let config = serde_yml::from_str::<ServerConfig>(
            r#"
            program-stream-max-start-delay: 1h
        "#,
        )
        .unwrap();
        config.validate();

        let config = serde_yml::from_str::<ServerConfig>(
            r#"
            program-stream-max-start-delay: 23h 59m 59s
        "#,
        )
        .unwrap();
        config.validate();
    }

    #[test]
    #[should_panic(
        expected = "config.server.program-stream-max-start-delay: must not be less than 24h"
    )]
    fn test_server_config_validate_max_start_delay_less_than_24h() {
        let config = serde_yml::from_str::<ServerConfig>(
            r#"
            program-stream-max-start-delay: 24h
        "#,
        )
        .unwrap();
        config.validate();
    }

    #[test]
    #[should_panic(expected = "config.server.addrs[0]: invalid socket address: invalid")]
    fn test_server_config_validate_addrs() {
        let mut config = ServerConfig::default();
        config.addrs = vec![ServerAddr::Http("invalid".to_string())];
        config.validate();
    }

    #[test]
    #[should_panic(
        expected = "config.server.folder-view-template-path: must be a path to an existing file"
    )]
    fn test_server_config_validate_folder_view_template_path() {
        let mut config = ServerConfig::default();
        config.folder_view_template_path = Some("not_found".into());
        config.validate();
    }

    #[test]
    fn test_mount_config_validate() {
        let config = MountConfig {
            path: env!("CARGO_MANIFEST_DIR").into(),
            index: None,
            listing: false,
        };
        config.validate("/test");
    }

    fn mount_config_validate_mount_point(mount_point: &str) {
        let config = MountConfig {
            path: env!("CARGO_MANIFEST_DIR").into(),
            index: None,
            listing: false,
        };
        config.validate(mount_point)
    }

    #[test]
    #[should_panic(expected = "config.server.mounts[]: a mount point must starts with '/'")]
    fn test_mount_config_validate_panic_mount_point_empty() {
        mount_config_validate_mount_point("")
    }

    #[test]
    #[should_panic(expected = r#"config.server.mounts[/]: cannot mount onto "/""#)]
    fn test_mount_config_validate_panic_mount_point_root() {
        mount_config_validate_mount_point("/")
    }

    #[test]
    #[should_panic(expected = r#"config.server.mounts[/api]: cannot mount onto "/api""#)]
    fn test_mount_config_validate_panic_mount_point_api() {
        mount_config_validate_mount_point("/api")
    }

    #[test]
    #[should_panic(expected = r#"config.server.mounts[/events]: cannot mount onto "/events""#)]
    fn test_mount_config_validate_panic_mount_point_events() {
        mount_config_validate_mount_point("/events")
    }

    #[test]
    #[should_panic(expected = "config.server.mounts[foobar]: a mount point must starts with '/'")]
    fn test_mount_config_validate_panic_mount_point_starts_with_slash() {
        mount_config_validate_mount_point("foobar")
    }

    #[test]
    #[should_panic(
        expected = "config.server.mounts[/foobar/]: a mount point must not ends with '/'"
    )]
    fn test_mount_config_validate_panic_mount_point_ends_with_slash() {
        mount_config_validate_mount_point("/foobar/")
    }

    #[test]
    #[should_panic(
        expected = "config.server.mounts[/test].path: must be a path to an existing entry"
    )]
    fn test_mount_config_validate_panic_path() {
        let config = MountConfig {
            path: "/path/not_found".into(),
            index: None,
            listing: false,
        };
        config.validate("/test")
    }

    #[test]
    #[should_panic(
        expected = "config.server.mounts[/test].index: must be an existing file if it exists"
    )]
    fn test_mount_config_validate_panic_index() {
        let config = MountConfig {
            path: env!("CARGO_MANIFEST_DIR").into(),
            index: Some("not_found".into()),
            listing: false,
        };
        config.validate("/test")
    }

    #[test]
    fn test_channel_config() {
        assert!(serde_yml::from_str::<ChannelConfig>("{}").is_err());

        fn channel_config() -> ChannelConfig {
            ChannelConfig {
                name: "x".to_string(),
                channel_type: ChannelType::GR,
                channel: "y".to_string(),
                extra_args: "".to_string(),
                services: vec![],
                excluded_services: vec![],
                disabled: false,
            }
        }

        let config = channel_config();
        assert_eq!(
            serde_yml::from_str::<ChannelConfig>(
                r#"
                name: x
                type: GR
                channel: y
            "#
            )
            .unwrap(),
            config
        );

        let mut config = channel_config();
        config.extra_args = "--extra args".to_string();
        assert_eq!(
            serde_yml::from_str::<ChannelConfig>(
                r#"
                name: x
                type: GR
                channel: y
                extra-args: "--extra args"
            "#
            )
            .unwrap(),
            config
        );

        let mut config = channel_config();
        config.disabled = true;
        assert_eq!(
            serde_yml::from_str::<ChannelConfig>(
                r#"
                name: x
                type: GR
                channel: y
                disabled: true
            "#
            )
            .unwrap(),
            config
        );

        let mut config = channel_config();
        config.excluded_services = vec![100.into()];
        assert_eq!(
            serde_yml::from_str::<ChannelConfig>(
                r#"
                name: x
                type: GR
                channel: y
                excluded-services: [100]
            "#
            )
            .unwrap(),
            config
        );

        assert!(serde_yml::from_str::<ChannelConfig>(
            r#"
                name: x
                type: WOWOW
                channel: y
            "#
        )
        .is_err());

        let result = serde_yml::from_str::<ChannelConfig>(
            r#"
            unknown:
              property: value
        "#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_channel_config_normalize() {
        fn channel_config() -> ChannelConfig {
            ChannelConfig {
                name: "ch".to_string(),
                channel_type: ChannelType::GR,
                channel: "1".to_string(),
                extra_args: "".to_string(),
                services: vec![],
                excluded_services: vec![],
                disabled: false,
            }
        }

        let config1 = channel_config();
        let mut config2 = channel_config();
        config2.channel = "2".to_string();
        assert_eq!(
            ChannelConfig::normalize(
                serde_yml::from_str::<Vec<ChannelConfig>>(
                    r#"
                - name: ch
                  type: GR
                  channel: 1
                - name: ch
                  type: GR
                  channel: 2
            "#
                )
                .unwrap()
            ),
            vec![config1, config2]
        );

        let mut config = channel_config();
        config.services = vec![1.into(), 2.into()];
        config.excluded_services = vec![3.into(), 4.into()];
        assert_eq!(
            ChannelConfig::normalize(
                serde_yml::from_str::<Vec<ChannelConfig>>(
                    r#"
                - name: ch
                  type: GR
                  channel: 1
                  services: [1]
                  excluded-services: [3]
                - name: ch
                  type: GR
                  channel: 1
                  services: [2]
                  excluded-services: [4]
            "#
                )
                .unwrap()
            ),
            vec![config]
        );

        let mut config = channel_config();
        config.services = vec![1.into(), 2.into()];
        config.excluded_services = vec![3.into(), 4.into()];
        assert_eq!(
            ChannelConfig::normalize(
                serde_yml::from_str::<Vec<ChannelConfig>>(
                    r#"
                - name: ch
                  type: GR
                  channel: 1
                  services: [2]
                  excluded-services: [4]
                - name: ch
                  type: GR
                  channel: 1
                  services: [1]
                  excluded-services: [3]
            "#
                )
                .unwrap()
            ),
            vec![config]
        );

        let mut config = channel_config();
        config.services = vec![1.into(), 2.into()];
        config.excluded_services = vec![3.into(), 4.into()];
        assert_eq!(
            ChannelConfig::normalize(
                serde_yml::from_str::<Vec<ChannelConfig>>(
                    r#"
                - name: ch
                  type: GR
                  channel: 1
                  services: [1]
                  excluded-services: [3]
                - name: ch
                  type: GR
                  channel: 1
                  services: [1, 2]
                  excluded-services: [3, 4]
            "#
                )
                .unwrap()
            ),
            vec![config]
        );

        let config = channel_config();
        assert_eq!(
            ChannelConfig::normalize(
                serde_yml::from_str::<Vec<ChannelConfig>>(
                    r#"
                - name: ch
                  type: GR
                  channel: 1
                - name: ch
                  type: GR
                  channel: 1
                  services: [2]
            "#
                )
                .unwrap()
            ),
            vec![config]
        );

        let config = channel_config();
        assert_eq!(
            ChannelConfig::normalize(
                serde_yml::from_str::<Vec<ChannelConfig>>(
                    r#"
                - name: ch
                  type: GR
                  channel: 1
                  services: [1]
                - name: ch
                  type: GR
                  channel: 1
            "#
                )
                .unwrap()
            ),
            vec![config]
        );
    }

    fn channel_config() -> ChannelConfig {
        ChannelConfig {
            name: "test".to_string(),
            channel_type: ChannelType::GR,
            channel: "test".to_string(),
            extra_args: "".to_string(),
            services: vec![],
            excluded_services: vec![],
            disabled: false,
        }
    }

    #[test]
    fn test_channel_config_validate() {
        let config = channel_config();
        config.validate(0);
    }

    #[test]
    #[should_panic(expected = "config.channels[0].name: must be a non-empty string")]
    fn test_channel_config_validate_empty_name() {
        let mut config = channel_config();
        config.name = "".to_string();
        config.validate(0);
    }

    #[test]
    #[should_panic(expected = "config.channels[0].channel: must be a non-empty string")]
    fn test_channel_config_validate_empty_channel() {
        let mut config = channel_config();
        config.channel = "".to_string();
        config.validate(0);
    }

    #[test]
    fn test_tuner_config() {
        assert!(serde_yml::from_str::<TunerConfig>("{}").is_err());

        let mut config = TunerConfig::default();
        config.name = "x".to_string();
        config.channel_types = vec![ChannelType::GR];
        config.command = "open tuner".to_string();
        assert_eq!(
            serde_yml::from_str::<TunerConfig>(
                r#"
                name: x
                types: [GR]
                command: open tuner
            "#
            )
            .unwrap(),
            config
        );

        let mut config = TunerConfig::default();
        config.name = "x".to_string();
        config.channel_types = vec![ChannelType::GR];
        config.command = "open tuner".to_string();
        config.time_limit = 1;
        assert_eq!(
            serde_yml::from_str::<TunerConfig>(
                r#"
                name: x
                types: [GR]
                command: open tuner
                time-limit: 1
            "#
            )
            .unwrap(),
            config
        );

        let mut config = TunerConfig::default();
        config.name = "x".to_string();
        config.channel_types = vec![ChannelType::GR];
        config.command = "open tuner".to_string();
        config.disabled = true;
        assert_eq!(
            serde_yml::from_str::<TunerConfig>(
                r#"
                name: x
                types: [GR]
                command: open tuner
                disabled: true
            "#
            )
            .unwrap(),
            config
        );

        let mut config = TunerConfig::default();
        config.name = "x".to_string();
        config.channel_types = vec![ChannelType::GR];
        config.command = "open tuner".to_string();
        config.decoded = true;
        assert_eq!(
            serde_yml::from_str::<TunerConfig>(
                r#"
                name: x
                types: [GR]
                command: open tuner
                decoded: true
            "#
            )
            .unwrap(),
            config
        );

        let mut config = TunerConfig::default();
        config.name = "x".to_string();
        config.channel_types = vec![
            ChannelType::GR,
            ChannelType::BS,
            ChannelType::CS,
            ChannelType::SKY,
        ];
        config.command = "open tuner".to_string();
        assert_eq!(
            serde_yml::from_str::<TunerConfig>(
                r#"
                name: x
                types: [GR, BS, CS, SKY]
                command: open tuner
            "#
            )
            .unwrap(),
            config
        );

        assert!(serde_yml::from_str::<TunerConfig>(
            r#"
                name: x
                types: [WOWOW]
                command: open tuner
            "#
        )
        .is_err());

        let result = serde_yml::from_str::<TunerConfig>(
            r#"
            unknown:
              property: value
        "#,
        );
        assert!(result.is_err());
    }

    fn tuner_config() -> TunerConfig {
        TunerConfig {
            name: "test".to_string(),
            channel_types: vec![ChannelType::GR],
            command: "test".to_string(),
            time_limit: TunerConfig::default_time_limit(),
            disabled: false,
            decoded: false,
        }
    }

    #[test]
    fn test_tuner_config_validate() {
        let config = tuner_config();
        config.validate(0);
    }

    #[test]
    #[should_panic(expected = "config.tuners[0].name: must be a non-empty string")]
    fn test_tuner_config_validate_empty_name() {
        let mut config = tuner_config();
        config.name = "".to_string();
        config.validate(0);
    }

    #[test]
    #[should_panic(expected = "config.tuners[0].types: must be a non-empty list")]
    fn test_tuner_config_validate_empty_types() {
        let mut config = tuner_config();
        config.channel_types = vec![];
        config.validate(0);
    }

    #[test]
    #[should_panic(expected = "config.tuners[0].command: must be a non-empty string")]
    fn test_tuner_config_validate_empty_command() {
        let mut config = tuner_config();
        config.command = "".to_string();
        config.validate(0);
    }

    #[test]
    fn test_tuner_config_validate_disabled() {
        let mut config = tuner_config();
        config.name = "".to_string();
        config.channel_types = vec![];
        config.command = "".to_string();
        config.disabled = true;
        config.validate(0);
    }

    #[test]
    fn test_filters_config() {
        assert_eq!(
            serde_yml::from_str::<FiltersConfig>("{}").unwrap(),
            Default::default()
        );

        let mut config = FiltersConfig::default();
        config.tuner_filter.command = "filter".to_string();
        assert_eq!(
            serde_yml::from_str::<FiltersConfig>(
                r#"
                tuner-filter:
                  command: filter
            "#
            )
            .unwrap(),
            config
        );

        let mut config = FiltersConfig::default();
        config.service_filter.command = "filter".to_string();
        assert_eq!(
            serde_yml::from_str::<FiltersConfig>(
                r#"
                service-filter:
                  command: filter
            "#
            )
            .unwrap(),
            config
        );

        let mut config = FiltersConfig::default();
        config.decode_filter.command = "filter".to_string();
        assert_eq!(
            serde_yml::from_str::<FiltersConfig>(
                r#"
                decode-filter:
                  command: filter
            "#
            )
            .unwrap(),
            config
        );

        let mut config = FiltersConfig::default();
        config.program_filter.command = "filter".to_string();
        assert_eq!(
            serde_yml::from_str::<FiltersConfig>(
                r#"
                program-filter:
                  command: filter
            "#
            )
            .unwrap(),
            config
        );

        let result = serde_yml::from_str::<FiltersConfig>(
            r#"
            unknown:
              property: value
        "#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_filter_config() {
        assert_eq!(
            serde_yml::from_str::<FilterConfig>("{}").unwrap(),
            Default::default()
        );

        let mut config = FilterConfig::default();
        config.command = "filter".to_string();
        assert_eq!(
            serde_yml::from_str::<FilterConfig>(
                r#"
                command: filter
            "#
            )
            .unwrap(),
            config
        );

        let result = serde_yml::from_str::<FilterConfig>(
            r#"
            unknown:
              property: value
        "#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_filter_config_validate() {
        let mut config = FilterConfig::default();
        config.command = "test".to_string();
        config.validate("filters", "test");
    }

    #[test]
    #[should_panic(expected = "config.filters[test].command: must be a non-empty string")]
    fn test_filter_config_validate_empty_command() {
        FilterConfig::default().validate("filters", "test");
    }

    #[test]
    fn test_post_filter_config() {
        assert_eq!(
            serde_yml::from_str::<PostFilterConfig>("{}").unwrap(),
            Default::default()
        );

        let mut config = PostFilterConfig::default();
        config.command = "filter".to_string();
        assert_eq!(
            serde_yml::from_str::<PostFilterConfig>(
                r#"
                command: filter
            "#
            )
            .unwrap(),
            config
        );

        let mut config = PostFilterConfig::default();
        config.command = "filter".to_string();
        config.content_type = Some("video/mp4".to_string());
        assert_eq!(
            serde_yml::from_str::<PostFilterConfig>(
                r#"
                command: filter
                content-type: video/mp4
            "#
            )
            .unwrap(),
            config
        );

        let result = serde_yml::from_str::<PostFilterConfig>(
            r#"
            unknown:
              property: value
        "#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_post_filter_config_validate() {
        let mut config = PostFilterConfig::default();
        config.command = "test".to_string();
        config.validate("test");
    }

    #[test]
    #[should_panic(expected = "config.post-filters[test].command: must be a non-empty string")]
    fn test_post_filter_config_validate_empty_command() {
        let mut config = PostFilterConfig::default();
        config.command = "".to_string();
        config.validate("test");
    }

    #[test]
    #[should_panic(expected = "config.post-filters[test].content-type: must be a non-empty string")]
    fn test_post_filter_config_validate_empty_content_type() {
        let mut config = PostFilterConfig::default();
        config.command = "test".to_string();
        config.content_type = Some("".to_string());
        config.validate("test");
    }

    #[test]
    fn test_jobs_config() {
        assert_eq!(
            serde_yml::from_str::<JobsConfig>("{}").unwrap(),
            Default::default()
        );

        let mut config = JobsConfig::default();
        config.scan_services.command = "job".to_string();
        config.scan_services.schedule = "*".to_string();
        assert_eq!(
            serde_yml::from_str::<JobsConfig>(
                r#"
                scan-services:
                  command: job
                  schedule: '*'
            "#
            )
            .unwrap(),
            config
        );

        let mut config = JobsConfig::default();
        config.sync_clocks.command = "job".to_string();
        config.sync_clocks.schedule = "*".to_string();
        assert_eq!(
            serde_yml::from_str::<JobsConfig>(
                r#"
                sync-clocks:
                  command: job
                  schedule: '*'
            "#
            )
            .unwrap(),
            config
        );

        let mut config = JobsConfig::default();
        config.update_schedules.command = "job".to_string();
        config.update_schedules.schedule = "*".to_string();
        assert_eq!(
            serde_yml::from_str::<JobsConfig>(
                r#"
                update-schedules:
                  command: job
                  schedule: '*'
            "#
            )
            .unwrap(),
            config
        );

        let result = serde_yml::from_str::<JobsConfig>(
            r#"
            unknown:
              property: value
        "#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_jobs_config_normalize() {
        assert_eq!(
            serde_yml::from_str::<JobsConfig>(
                r#"
                scan-services:
                  command: ''
                  schedule: ''
                sync-clocks:
                  command: ''
                  schedule: ''
                update-schedules:
                  command: ''
                  schedule: ''
            "#
            )
            .unwrap()
            .normalize(),
            JobsConfig::default()
        );
    }

    #[test]
    fn test_job_config() {
        fn job_config() -> JobConfig {
            JobConfig {
                command: "".to_string(),
                schedule: "".to_string(),
                disabled: false,
            }
        }

        assert_eq!(
            serde_yml::from_str::<JobConfig>("{}").unwrap(),
            job_config()
        );

        let mut config = job_config();
        config.command = "test".to_string();
        assert_eq!(
            serde_yml::from_str::<JobConfig>(
                r#"
                command: test
            "#
            )
            .unwrap(),
            config
        );

        let mut config = job_config();
        config.schedule = "*".to_string();
        assert_eq!(
            serde_yml::from_str::<JobConfig>(
                r#"
                schedule: '*'
            "#
            )
            .unwrap(),
            config
        );

        let mut config = job_config();
        config.disabled = true;
        assert_eq!(
            serde_yml::from_str::<JobConfig>(
                r#"
                disabled: true
            "#
            )
            .unwrap(),
            config
        );

        assert!(serde_yml::from_str::<JobConfig>(
            r#"
            unknown:
              property: value
        "#
        )
        .is_err());
    }

    fn job_config() -> JobConfig {
        JobConfig {
            command: "test".to_string(),
            schedule: "0 30 9,12,15 1,15 May-Aug Mon,Wed,Fri 2018/2".to_string(),
            disabled: false,
        }
    }

    #[test]
    fn test_job_config_validate() {
        let config = job_config();
        config.validate("test");
    }

    #[test]
    #[should_panic(expected = "config.jobs[test].command: must be a non-empty string")]
    fn test_job_config_validate_command() {
        let mut config = job_config();
        config.command = "".to_string();
        config.validate("test");
    }

    #[test]
    #[should_panic(expected = "config.jobs[test].schedule: not valid")]
    fn test_job_config_validate_schedule() {
        let mut config = job_config();
        config.schedule = "".to_string();
        config.validate("test");
    }

    #[test]
    fn test_job_config_validate_disabled() {
        let mut config = job_config();
        config.command = "".to_string();
        config.schedule = "".to_string();
        config.disabled = true;
        config.validate("test");
    }

    #[test]
    fn test_recording_config() {
        assert_eq!(
            serde_yml::from_str::<RecordingConfig>("{}").unwrap(),
            Default::default()
        );

        assert_eq!(
            serde_yml::from_str::<RecordingConfig>("basedir: /tmp").unwrap(),
            RecordingConfig {
                basedir: Some("/tmp".into()),
                ..Default::default()
            }
        );
    }

    #[test]
    fn test_recording_is_enabled() {
        assert!(!RecordingConfig::default().is_enabled());
        assert!(serde_yml::from_str::<RecordingConfig>("basedir: /tmp")
            .unwrap()
            .is_enabled());
    }

    #[test]
    fn test_recording_config_validate_basedir() {
        let mut config = RecordingConfig::default();
        config.basedir = Some("/tmp".into());
        config.validate();
    }

    #[test]
    #[should_panic(expected = "config.recording.basedir: must be an absolute path")]
    fn test_recording_config_validate_basedir_relative() {
        let mut config = RecordingConfig::default();
        config.basedir = Some("relative/dir".into());
        config.validate();
    }

    #[test]
    #[should_panic(expected = "config.recording.basedir: must be a path to an existing directory")]
    fn test_recording_config_validate_basedir_not_existing() {
        let mut config = RecordingConfig::default();
        config.basedir = Some("/no/such/dir".into());
        config.validate();
    }

    #[test]
    fn test_timeshift_config() {
        assert_eq!(
            serde_yml::from_str::<TimeshiftConfig>("{}").unwrap(),
            Default::default()
        );

        assert_eq!(
            serde_yml::from_str::<TimeshiftConfig>(
                r#"
                command: command
                recorders:
                  test:
                    service-id: 1
                    ts-file: /path/to/timeshift.m2ts
                    data-file: /path/to/timeshift.json
                    num-chunks: 10
                    uses:
                      tuner: tuner
                      channel-type: GR
                      channel: ch
            "#
            )
            .unwrap(),
            TimeshiftConfig {
                command: "command".to_string(),
                recorders: indexmap! {
                    "test".to_string() => timeshift_recorder_config(),
                },
            },
        );

        assert!(serde_yml::from_str::<TimeshiftConfig>(
            r#"
            unknown: property
        "#
        )
        .is_err());
    }

    #[test]
    fn test_timeshift_is_enabled() {
        assert!(!TimeshiftConfig::default().is_enabled());

        assert!(serde_yml::from_str::<TimeshiftConfig>(
            r#"
                command: command
                recorders:
                  test:
                    service-id: 1
                    ts-file: /path/to/timeshift.m2ts
                    data-file: /path/to/timeshift.json
                    num-chunks: 100
                    uses:
                      tuner: tuner
                      channel-type: GR
                      channel: ch
            "#
        )
        .unwrap()
        .is_enabled());

        assert!(!serde_yml::from_str::<TimeshiftConfig>(
            r#"
                command: ''
                recorders:
                  test:
                    service-id: 1
                    ts-file: /path/to/timeshift.m2ts
                    data-file: /path/to/timeshift.json
                    num-chunks: 100
                    uses:
                      tuner: tuner
                      channel-type: GR
                      channel: ch
            "#
        )
        .unwrap()
        .is_enabled());
    }

    #[test]
    fn test_timeshift_config_validate() {
        let config = TimeshiftConfig::default();
        config.validate();
    }

    #[test]
    #[should_panic(expected = "config.timeshift.command: must be a non-empty string")]
    fn test_timeshift_config_validate_empty_command() {
        let mut config = TimeshiftConfig::default();
        config.command = "".to_string();
        config.validate();
    }

    fn timeshift_recorder_config() -> TimeshiftRecorderConfig {
        TimeshiftRecorderConfig {
            service_id: 1.into(),
            ts_file: "/path/to/timeshift.m2ts".into(),
            data_file: "/path/to/timeshift.json".into(),
            chunk_size: TimeshiftRecorderConfig::default_chunk_size(),
            num_chunks: 10,
            num_reserves: TimeshiftRecorderConfig::default_num_reserves(),
            priority: TimeshiftRecorderConfig::default_priority(),
            uses: TimeshiftRecorderUses {
                tuner: "tuner".to_string(),
                channel_type: ChannelType::GR,
                channel: "ch".to_string(),
            },
        }
    }

    #[test]
    fn test_timeshift_recorder_config() {
        assert!(serde_yml::from_str::<TimeshiftRecorderConfig>("{}").is_err());

        assert!(serde_yml::from_str::<TimeshiftRecorderConfig>(
            r#"
            service-id: 1
            ts-file: /path/to/timeshift.m2ts
            data-file: /path/to/timeshift.json
            num-chunks: 10
            uses:
              tuner: tuner
              channel-type: GR
              channel: ch
            unknown: property
        "#
        )
        .is_err());

        assert_eq!(
            serde_yml::from_str::<TimeshiftRecorderConfig>(
                r#"
                service-id: 1
                ts-file: /path/to/timeshift.m2ts
                data-file: /path/to/timeshift.json
                num-chunks: 10
                uses:
                  tuner: tuner
                  channel-type: GR
                  channel: ch
            "#
            )
            .unwrap(),
            timeshift_recorder_config()
        );

        assert_eq!(
            serde_yml::from_str::<TimeshiftRecorderConfig>(
                r#"
                service-id: 2
                ts-file: /1.m2ts
                data-file: /1.json
                chunk-size: 8192
                num-chunks: 100
                num-reserves: 2
                priority: 2
                uses:
                  tuner: x
                  channel-type: CS
                  channel: x
            "#
            )
            .unwrap(),
            TimeshiftRecorderConfig {
                service_id: 2.into(),
                ts_file: "/1.m2ts".into(),
                data_file: "/1.json".into(),
                chunk_size: 8192,
                num_chunks: 100,
                num_reserves: 2,
                priority: 2,
                uses: TimeshiftRecorderUses {
                    tuner: "x".to_string(),
                    channel_type: ChannelType::CS,
                    channel: "x".to_string(),
                },
            }
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn test_timeshift_recorder_config_validate() {
        // NOTE: The chunk size for this test should be the default chunk size,
        //       but it's too large to allocate in /tmp...
        let chunk_size = TimeshiftRecorderConfig::BUFSIZE;
        let num_chunks = 3;
        let ts_file = NamedTempFile::new().unwrap();
        let ts_file_size = (chunk_size * num_chunks) as libc::off64_t;
        unsafe {
            let _ = libc::fallocate64(ts_file.as_raw_fd(), 0, 0, ts_file_size);
        }
        let data_file = tempfile::Builder::new().suffix(".json").tempfile().unwrap();
        let mut config = timeshift_recorder_config();
        ts_file.path().clone_into(&mut config.ts_file);
        data_file.path().clone_into(&mut config.data_file);
        config.chunk_size = chunk_size;
        config.num_chunks = num_chunks;
        config.validate("test");
    }

    #[test]
    #[should_panic(
        expected = "config.timeshift.recorders[test]: `ts-file` must be an absolute path"
    )]
    fn test_timeshift_recorder_config_validate_empty_ts_file() {
        let mut config = timeshift_recorder_config();
        config.ts_file = "".into();
        config.validate("test");
    }

    #[test]
    #[should_panic(
        expected = "config.timeshift.recorders[test]: `ts-file` must be a path to a regular file"
    )]
    fn test_timeshift_recorder_config_validate_ts_file_is_file() {
        let mut config = timeshift_recorder_config();
        config.ts_file = "/dev/null".into();
        config.validate("test");
    }

    #[test]
    #[should_panic(
        expected = "config.timeshift.recorders[test]: `data-file` must be an absolute path"
    )]
    fn test_timeshift_recorder_config_validate_empty_data_file() {
        let ts_file = NamedTempFile::new().unwrap();
        let mut config = timeshift_recorder_config();
        ts_file.path().clone_into(&mut config.ts_file);
        config.data_file = "".into();
        config.validate("test");
    }

    #[test]
    #[should_panic(
        expected = "config.timeshift.recorders[test]: `data-file` must be a path to a regular file"
    )]
    fn test_timeshift_recorder_config_validate_data_file_is_file() {
        let ts_file = NamedTempFile::new().unwrap();
        let mut config = timeshift_recorder_config();
        ts_file.path().clone_into(&mut config.ts_file);
        config.data_file = "/dev/null".into();
        config.validate("test");
    }

    #[test]
    #[should_panic(expected = "config.timeshift.recorders[test]: `data-file` must be a JSON file")]
    fn test_timeshift_recorder_config_validate_data_bincode() {
        let ts_file = NamedTempFile::new().unwrap();
        let data_file = tempfile::Builder::new()
            .suffix(".bincode")
            .tempfile()
            .unwrap();
        let mut config = timeshift_recorder_config();
        ts_file.path().clone_into(&mut config.ts_file);
        data_file.path().clone_into(&mut config.data_file);
        config.validate("test");
    }

    #[test]
    #[should_panic(
        expected = "config.timeshift.recorders[test]: `chunk-size` must be larger than 0"
    )]
    fn test_timeshift_recorder_config_validate_chunk_size_0() {
        let ts_file = NamedTempFile::new().unwrap();
        let data_file = tempfile::Builder::new().suffix(".json").tempfile().unwrap();
        let mut config = timeshift_recorder_config();
        ts_file.path().clone_into(&mut config.ts_file);
        data_file.path().clone_into(&mut config.data_file);
        config.chunk_size = 0;
        config.validate("test");
    }

    #[test]
    #[should_panic(
        expected = "config.timeshift.recorders[test]: `chunk-size` must be a multiple of 8192"
    )]
    fn test_timeshift_recorder_config_validate_chunk_size_1() {
        let ts_file = NamedTempFile::new().unwrap();
        let data_file = tempfile::Builder::new().suffix(".json").tempfile().unwrap();
        let mut config = timeshift_recorder_config();
        ts_file.path().clone_into(&mut config.ts_file);
        data_file.path().clone_into(&mut config.data_file);
        config.chunk_size = 1;
        config.validate("test");
    }

    #[test]
    #[should_panic(
        expected = "config.timeshift.recorders[test]: `num-chunks` must be larger than 2"
    )]
    fn test_timeshift_recorder_config_validate_num_chunks_2() {
        let ts_file = NamedTempFile::new().unwrap();
        let data_file = tempfile::Builder::new().suffix(".json").tempfile().unwrap();
        let mut config = timeshift_recorder_config();
        ts_file.path().clone_into(&mut config.ts_file);
        data_file.path().clone_into(&mut config.data_file);
        config.num_chunks = 2;
        config.validate("test");
    }

    #[test]
    #[should_panic(
        expected = "config.timeshift.recorders[test]: `num-reserves` must be larger than 0"
    )]
    fn test_timeshift_recorder_config_validate_num_reserves_0() {
        let ts_file = NamedTempFile::new().unwrap();
        let data_file = tempfile::Builder::new().suffix(".json").tempfile().unwrap();
        let mut config = timeshift_recorder_config();
        ts_file.path().clone_into(&mut config.ts_file);
        data_file.path().clone_into(&mut config.data_file);
        config.num_reserves = 0;
        config.validate("test");
    }

    #[test]
    #[should_panic(
        expected = "config.timeshift.recorders[test]: Maximum number of available chunks (`num-chunks` - `num-reserves`) must be larger than 1"
    )]
    fn test_timeshift_recorder_config_validate_available_chunks_1() {
        let ts_file = NamedTempFile::new().unwrap();
        let data_file = tempfile::Builder::new().suffix(".json").tempfile().unwrap();
        let mut config = timeshift_recorder_config();
        ts_file.path().clone_into(&mut config.ts_file);
        data_file.path().clone_into(&mut config.data_file);
        config.num_chunks = 3;
        config.num_reserves = 2;
        config.validate("test");
    }

    #[test]
    #[should_panic(
        expected = "config.timeshift.recorders[test].uses.tuner: must be a non-empty string"
    )]
    fn test_timeshift_recorder_config_validate_uses_empty_tuner() {
        let ts_file = NamedTempFile::new().unwrap();
        let data_file = tempfile::Builder::new().suffix(".json").tempfile().unwrap();
        let mut config = timeshift_recorder_config();
        ts_file.path().clone_into(&mut config.ts_file);
        data_file.path().clone_into(&mut config.data_file);
        config.uses.tuner = "".to_string();
        config.validate("test");
    }

    #[test]
    #[should_panic(
        expected = "config.timeshift.recorders[test].uses.channel: must be a non-empty string"
    )]
    fn test_timeshift_recorder_config_validate_uses_empty_channel() {
        let ts_file = NamedTempFile::new().unwrap();
        let data_file = tempfile::Builder::new().suffix(".json").tempfile().unwrap();
        let mut config = timeshift_recorder_config();
        ts_file.path().clone_into(&mut config.ts_file);
        data_file.path().clone_into(&mut config.data_file);
        config.uses.channel = "".to_string();
        config.validate("test");
    }

    #[test]
    #[should_panic(
        expected = "config.timeshift.recorders[test]: `ts-file` must be allocated with 1540096000 in advance"
    )]
    fn test_timeshift_recorder_config_validate_ts_file_size() {
        let ts_file = NamedTempFile::new().unwrap();
        let data_file = tempfile::Builder::new().suffix(".json").tempfile().unwrap();
        let mut config = timeshift_recorder_config();
        ts_file.path().clone_into(&mut config.ts_file);
        data_file.path().clone_into(&mut config.data_file);
        config.validate("test");
    }

    fn local_onair_program_tracker_config() -> LocalOnairProgramTrackerConfig {
        LocalOnairProgramTrackerConfig {
            channel_types: hashset![ChannelType::GR],
            services: Default::default(),
            excluded_services: Default::default(),
            command: LocalOnairProgramTrackerConfig::default_command(),
            uses: LocalOnairProgramTrackerUses {
                tuner: "tuner".to_string(),
            },
            stream_id: None,
        }
    }

    #[test]
    fn test_local_onair_program_tracker_config() {
        assert_eq!(
            serde_yml::from_str::<LocalOnairProgramTrackerConfig>(
                r#"
                channel-types: [GR]
                uses:
                  tuner: tuner
            "#
            )
            .unwrap(),
            local_onair_program_tracker_config()
        );

        let mut config = local_onair_program_tracker_config();
        config.services = hashset![1.into()];
        config.excluded_services = hashset![2.into()];
        config.command = "true".to_string();
        assert_eq!(
            serde_yml::from_str::<LocalOnairProgramTrackerConfig>(
                r#"
                channel-types: [GR]
                services: [1]
                excluded-services: [2]
                command: "true"
                uses:
                  tuner: tuner
            "#
            )
            .unwrap(),
            config
        );
    }

    #[test]
    fn test_local_onair_program_tracker_config_validate() {
        let config = local_onair_program_tracker_config();
        config.validate("test");

        let mut config = local_onair_program_tracker_config();
        config.services = hashset![1.into()];
        config.excluded_services = hashset![2.into()];
        config.command = "true".to_string();
        config.validate("test");
    }

    #[test]
    #[should_panic(
        expected = "config.onair-program-trackers[test].channel-types: must be a non-empty list"
    )]
    fn test_local_onair_program_tracker_config_validate_channel_types() {
        let mut config = local_onair_program_tracker_config();
        config.channel_types = hashset![];
        config.validate("test");
    }

    #[test]
    #[should_panic(
        expected = "config.onair-program-trackers[test]command: must be a non-empty string"
    )]
    fn test_local_onair_program_tracker_config_validate_command() {
        let mut config = local_onair_program_tracker_config();
        config.command = "".to_string();
        config.validate("test");
    }

    #[test]
    #[should_panic(
        expected = "config.onair-program-trackers[test].uses.tuner: must be a non-empty string"
    )]
    fn test_local_onair_program_tracker_config_validate_uses() {
        let mut config = local_onair_program_tracker_config();
        config.uses.tuner = "".to_string();
        config.validate("test");
    }

    #[test]
    fn test_remote_onair_program_tracker_config_events_url() {
        let config = RemoteOnairProgramTrackerConfig {
            events_endpoint: "/test".to_string(),
            ..Default::default()
        };
        assert_eq!(
            config.events_url(),
            Url::parse("http://localhost:40772/test").unwrap()
        );
    }

    #[test]
    fn test_remote_onair_program_tracker_config_onair_url() {
        let config = RemoteOnairProgramTrackerConfig {
            onair_endpoint: "/test".to_string(),
            ..Default::default()
        };
        assert_eq!(
            config.onair_url(),
            Url::parse("http://localhost:40772/test").unwrap()
        );
    }

    #[test]
    fn test_remote_onair_program_tracker_config_onair_url_of() {
        let config = RemoteOnairProgramTrackerConfig::default();
        assert_eq!(
            config.onair_url_of(1.into()),
            Url::parse("http://localhost:40772/api/onair/1").unwrap()
        );
    }

    #[test]
    fn test_resource_config() {
        assert_eq!(
            serde_yml::from_str::<ResourceConfig>("{}").unwrap(),
            Default::default()
        );

        assert_eq!(
            serde_yml::from_str::<ResourceConfig>(
                r#"
                strings-yaml: /path/to/strings.yml
            "#
            )
            .unwrap(),
            ResourceConfig {
                strings_yaml: "/path/to/strings.yml".to_string(),
                ..Default::default()
            }
        );

        assert_eq!(
            serde_yml::from_str::<ResourceConfig>(
                r#"
                logos:
                  - service-id: 1
                    image: /path/to/logo.png
            "#
            )
            .unwrap(),
            ResourceConfig {
                logos: hashmap! {
                    1.into() => "/path/to/logo.png".to_string(),
                },
                ..Default::default()
            }
        );

        let result = serde_yml::from_str::<ResourceConfig>(
            r#"
            unknown:
              property: value
        "#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_resource_config_validate_existing_strings_yaml() {
        let mut config = ResourceConfig::default();
        config.strings_yaml = "/bin/sh".to_string();
        config.validate();
    }

    #[test]
    #[should_panic(
        expected = "config.resources.strings-yaml: must be a path to an existing YAML file"
    )]
    fn test_resource_config_validate_non_existing_strings_yaml() {
        let mut config = ResourceConfig::default();
        config.strings_yaml = "/path/to/non-existing".to_string();
        config.validate();
    }

    #[test]
    fn test_resource_config_validate_existing_logos() {
        let mut config = ResourceConfig::default();
        config.strings_yaml = "/bin/sh".to_string();
        config.logos = hashmap! {
            1.into() => "/bin/sh".to_string(),
        };
        config.validate();
    }

    #[test]
    #[should_panic(expected = "config.resources.logos[1]: must be a path to an existing file")]
    fn test_epg_config_validate_non_existing_logos() {
        let mut config = ResourceConfig::default();
        config.strings_yaml = "/bin/sh".to_string();
        config.logos = hashmap! {
            1.into() => "/path/to/non-existing".to_string(),
        };
        config.validate();
    }
}
