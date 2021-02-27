use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use actix::prelude::*;
use log;
use mustache;

use crate::broadcaster::*;
use crate::command_util::{spawn_pipeline, CommandPipeline};
use crate::config::{Config, FilterConfig, TunerConfig};
use crate::epg::EpgChannel;
use crate::error::Error;
use crate::models::*;
use crate::mpeg_ts_stream::MpegTsStream;

pub fn start(config: Arc<Config>) -> Addr<TunerManager> {
    TunerManager::new(config).start()
}

// identifiers

type TunerStream = MpegTsStream<TunerSubscriptionId, BroadcasterStream>;

#[derive(Clone, Copy, PartialEq)]
#[cfg_attr(test, derive(Debug, Default))]
pub struct TunerSessionId {
    tuner_index: usize,
    session_number: u32,
}

impl TunerSessionId {
    pub fn new(tuner_index: usize) -> Self {
        static COUNTER:AtomicU32 = AtomicU32::new(0);
        let session_number = COUNTER.fetch_add(1, Ordering::Relaxed);
        TunerSessionId { tuner_index, session_number }
    }
}

impl fmt::Display for TunerSessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "tuner#{}.{}", self.tuner_index, self.session_number)
    }
}

#[derive(Clone, Copy, PartialEq)]
#[cfg_attr(test, derive(Default))]
pub struct TunerSubscriptionId {
    session_id: TunerSessionId,
    serial_number: u32,
}

impl TunerSubscriptionId {
    pub fn new(session_id: TunerSessionId, serial_number: u32) -> Self {
        Self { session_id, serial_number }
    }
}

impl fmt::Display for TunerSubscriptionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.session_id, self.serial_number)
    }
}

// tuner manager

pub struct TunerManager {
    config: Arc<Config>,
    tuners: Vec<Tuner>,
}

struct TunerSubscription {
    id: TunerSubscriptionId,
    broadcaster: Addr<Broadcaster>,
    decoded: bool,
}

impl TunerSubscription {
    fn new(id: TunerSubscriptionId, broadcaster: Addr<Broadcaster>) -> Self {
        Self { id, broadcaster, decoded: false, }
    }
}

impl TunerManager {
    pub fn new(config: Arc<Config>) -> Self {
        TunerManager { config, tuners: Vec::new() }
    }

    fn load_tuners(&mut self) {
        log::info!("Loading tuners...");
        let tuners: Vec<Tuner> = self.config
            .tuners
            .iter()
            .filter(|config| !config.disabled)
            .enumerate()
            .map(|(i, config)| Tuner::new(i, config))
            .collect();
        log::info!("Loaded {} tuners", tuners.len());
        self.tuners = tuners;
    }

    fn activate_tuner(
        &mut self,
        channel: EpgChannel,
        user: TunerUser,
    ) -> Result<TunerSubscription, Error> {
        if let TunerUserInfo::Tracker { stream_id } = user.info {
            let tuner = &mut self.tuners[stream_id.session_id.tuner_index];
            if tuner.is_active() {
                return Ok(tuner.subscribe(user));
            }
            return Err(Error::TunerUnavailable);
        }

        let found = self.tuners
            .iter_mut()
            .find(|tuner| tuner.is_reuseable(&channel));
        if let Some(tuner) = found {
            log::info!("tuner#{}: Reuse tuner already activated for {}",
                       tuner.index, channel);
            return Ok(tuner.subscribe(user));
        }

        // Clone the config in order to avoid compile errors from the borrow checker.
        let config = self.config.clone();

        let found = self.tuners
            .iter_mut()
            .find(|tuner| tuner.is_available_for(&channel));
        if let Some(tuner) = found {
            log::info!("tuner#{}: Activate for {}", tuner.index, channel);
            let filters = Self::make_filter_commands(
                &tuner, &channel, &config.filters.tuner_filter)?;
            tuner.activate(channel, filters)?;
            return Ok(tuner.subscribe(user));
        }

        // No available tuner at this point.
        // Grab a tuner used by lower priority users.
        let found = self.tuners
            .iter_mut()
            .filter(|tuner| tuner.is_supported_type(&channel))
            .find(|tuner| tuner.can_grab(user.priority));
        if let Some(tuner) = found {
            log::info!("tuner#{}: Grab tuner, reactivate for {}", tuner.index, channel);
            let filters = Self::make_filter_commands(
                &tuner, &channel, &config.filters.tuner_filter)?;
            tuner.deactivate();
            tuner.activate(channel, filters)?;
            return Ok(tuner.subscribe(user));
        }

        log::warn!("No tuner available for {} {}", channel, user);
        Err(Error::TunerUnavailable)
    }

    fn deactivate_tuner(&mut self, id: TunerSubscriptionId) {
        log::info!("tuner#{}: Deactivate", id.session_id.tuner_index);
        self.tuners[id.session_id.tuner_index].deactivate();
    }

    fn stop_streaming(&mut self, id: TunerSubscriptionId) {
        log::info!("{}: Stop streaming", id);
        let _ = self.tuners[id.session_id.tuner_index].stop_streaming(id);
    }

    fn make_filter_commands(
        tuner: &Tuner,
        channel: &EpgChannel,
        filter: &FilterConfig,
    ) -> Result<Vec<String>, Error> {
        let filter = Self::make_filter_command(tuner, channel, &filter.command)?;
        if filter.trim().is_empty() {
            Ok(vec![])
        } else {
            Ok(vec![filter])
        }
    }

    fn make_filter_command(
        tuner: &Tuner,
        channel: &EpgChannel,
        filter: &str,
    ) -> Result<String, Error> {
        let template = mustache::compile_str(filter)?;
        let data = mustache::MapBuilder::new()
            .insert("tuner_index", &tuner.index)?
            .insert_str("tuner_name", &tuner.name)
            .insert_str("channel_name", &channel.name)
            .insert("channel_type", &channel.channel_type)?
            .insert_str("channel", &channel.channel)
            .build();
        Ok(template.render_data_to_string(&data)?)
    }
}

impl Actor for TunerManager {
    type Context = actix::Context<Self>;

    fn started(&mut self, _: &mut Self::Context) {
        // It's guaranteed that no response is sent before tuners are loaded.
        log::debug!("Started");
        self.load_tuners();
    }

    fn stopped(&mut self, _: &mut Self::Context) {
        for tuner in self.tuners.iter_mut() {
            tuner.deactivate();
        }
        log::debug!("Stopped");
    }
}

// query tuners

pub struct QueryTunersMessage;

impl fmt::Display for QueryTunersMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "QueryTuners")
    }
}

impl Message for QueryTunersMessage {
    type Result = Result<Vec<MirakurunTuner>, Error>;
}

impl Handler<QueryTunersMessage> for TunerManager {
    type Result = Result<Vec<MirakurunTuner>, Error>;

    fn handle(
        &mut self,
        msg: QueryTunersMessage,
        _: &mut Self::Context,
    ) -> Self::Result {
        log::debug!("{}", msg);
        let tuners: Vec<MirakurunTuner> = self.tuners
            .iter()
            .map(|tuner| tuner.get_mirakurun_model())
            .collect();
        Ok(tuners)
    }
}

// start streaming

pub struct StartStreamingMessage {
    pub channel: EpgChannel,
    pub user: TunerUser,
}

impl fmt::Display for StartStreamingMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "StartStreaming {} to {}", self.channel, self.user)
    }
}

impl Message for StartStreamingMessage {
    type Result = Result<TunerStream, Error>;
}

impl Handler<StartStreamingMessage> for TunerManager {
    type Result = ActorResponse<Self, TunerStream, Error>;

    fn handle(
        &mut self,
        msg: StartStreamingMessage,
        _: &mut Self::Context,
    ) -> Self::Result {
        log::debug!("{}", msg);

        let subscription = match self.activate_tuner(msg.channel, msg.user) {
            Ok(subscription) => subscription,
            Err(err) => return ActorResponse::reply(Err(Error::from(err))),
        };

        let fut = actix::fut::wrap_future::<_, Self>(
            subscription.broadcaster.send(SubscribeMessage {
                id: subscription.id
            }))
            .map(move |result, act, _ctx| {
                if result.is_ok() {
                    log::info!("{}: Started streaming", subscription.id);
                } else {
                    log::error!("{}: Broadcaster may have stopped",
                                subscription.id);
                    act.deactivate_tuner(subscription.id);
                }
                result
                    .map(|stream| MpegTsStream::new(subscription.id, stream))
                    .map(|stream| if subscription.decoded { stream.decoded() } else { stream })
                    .map_err(Error::from)
            });

        ActorResponse::r#async(fut)
    }
}

// stop streaming

pub struct StopStreamingMessage {
    pub id: TunerSubscriptionId,
}

impl fmt::Display for StopStreamingMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "StopStreaming {}", self.id)
    }
}

impl Message for StopStreamingMessage {
    type Result = ();
}

impl Handler<StopStreamingMessage> for TunerManager {
    type Result = ();

    fn handle(
        &mut self,
        msg: StopStreamingMessage,
        _: &mut Self::Context,
    ) -> Self::Result {
        log::debug!("{}", msg);
        self.stop_streaming(msg.id)
    }
}

// tuner

struct Tuner {
    index: usize,
    name: String,
    channel_types: Vec<ChannelType>,
    command: String,
    time_limit: u64,
    decoded: bool,
    activity: TunerActivity,
}

impl Tuner {
    fn new(
        index: usize,
        config: &TunerConfig,
    ) -> Self {
        Tuner {
            index,
            name: config.name.clone(),
            channel_types: config.channel_types.clone(),
            command: config.command.clone(),
            time_limit: config.time_limit,
            decoded: config.decoded,
            activity: TunerActivity::Inactive,
        }
    }

    fn is_active(&self) -> bool {
        self.activity.is_active()
    }

    fn is_available(&self) -> bool {
        self.activity.is_inactive()
    }

    fn is_supported_type(&self, channel: &EpgChannel) -> bool {
        self.channel_types.contains(&channel.channel_type)
    }

    fn is_available_for(&self, channel: &EpgChannel) -> bool {
        self.is_available() && self.is_supported_type(channel)
    }

    fn is_reuseable(&self, channel: &EpgChannel) -> bool {
        self.activity.is_reuseable(channel)
    }

    fn can_grab(&self, priority: TunerUserPriority) -> bool {
        priority.is_grab() || self.activity.can_grab(priority)
    }

    fn activate(
        &mut self,
        channel: EpgChannel,
        filters: Vec<String>,
    ) -> Result<(), Error> {
        let command = self.make_command(&channel)?;
        self.activity.activate(
            self.index, channel, command, filters, self.time_limit)
    }

    fn deactivate(&mut self) {
        self.activity.deactivate();
    }

    fn subscribe(&mut self, user: TunerUser) -> TunerSubscription {
        let mut subscription = self.activity.subscribe(user);
        subscription.decoded = self.decoded;
        subscription
    }

    fn stop_streaming(
        &mut self,
        id: TunerSubscriptionId,
    ) -> Result<(), Error> {
        let num_users = self.activity.stop_streaming(id)?;
        if num_users == 0 {
            self.deactivate();
        }
        Ok(())
    }

    fn get_mirakurun_model(&self) -> MirakurunTuner {
        let (command, pid, users) = self.activity.get_mirakurun_models();

        MirakurunTuner {
            index: self.index,
            name: self.name.clone(),
            channel_types: self.channel_types.clone(),
            command,
            pid,
            users,
            is_available: true,
            is_remote: false,
            is_free: self.is_available(),
            is_using: !self.is_available(),
            is_fault: false,
        }
    }

    fn make_command(&self, channel: &EpgChannel) -> Result<String, Error> {
        let template = mustache::compile_str(&self.command)?;
        let data = mustache::MapBuilder::new()
            .insert("channel_type", &channel.channel_type)?
            .insert_str("channel", &channel.channel)
            .insert_str("extra_args", &channel.extra_args)
            .insert_str("duration", "-")
            .build();
        Ok(template.render_data_to_string(&data)?)
    }
}

// activity

enum TunerActivity {
    Inactive,
    Active(TunerSession),
}

impl TunerActivity {
    fn activate(
        &mut self,
        tuner_index: usize,
        channel: EpgChannel,
        command: String,
        filters: Vec<String>,
        time_limit: u64,
    ) -> Result<(), Error> {
        match self {
            Self::Inactive => {
                let session = TunerSession::new(
                    tuner_index, channel, command, filters, time_limit)?;
                *self = Self::Active(session);
                Ok(())
            }
            Self::Active(_) => panic!("Must be deactivated before activating"),
        }
    }

    fn deactivate(&mut self) {
        *self = Self::Inactive;
    }

    fn is_active(&self) -> bool {
        match self {
            Self::Inactive => false,
            Self::Active(_) => true,
        }
    }

    fn is_inactive(&self) -> bool {
        !self.is_active()
    }

    fn is_reuseable(&self, channel: &EpgChannel) -> bool {
        match self {
            Self::Inactive => false,
            Self::Active(session) => session.is_reuseable(channel),
        }
    }

    fn subscribe(&mut self, user: TunerUser) -> TunerSubscription {
        match self {
            Self::Inactive => panic!("Must be activated before subscribing"),
            Self::Active(session) => session.subscribe(user),
        }
    }

    fn stop_streaming(
        &mut self,
        id: TunerSubscriptionId,
    ) -> Result<usize, Error> {
        match self {
            Self::Inactive => Err(Error::SessionNotFound),
            Self::Active(session) => session.stop_streaming(id),
        }
    }

    fn can_grab(&self, priority: TunerUserPriority) -> bool {
        match self {
            Self::Inactive => true,
            Self::Active(session) => session.can_grab(priority),
        }
    }

    fn get_mirakurun_models(
        &self
    ) -> (Option<String>, Option<u32>, Vec<MirakurunTunerUser>) {
        match self {
            Self::Inactive => (None, None, Vec::new()),
            Self::Active(session) => session.get_mirakurun_models(),
        }
    }
}

// session

struct TunerSession {
    id: TunerSessionId,
    channel: EpgChannel,
    command: String,
    // Used for closing the tuner in order to take over the right to use it.
    pipeline: CommandPipeline<TunerSessionId>,
    broadcaster: Addr<Broadcaster>,
    subscribers: HashMap<u32, TunerUser>,
    next_serial_number: u32,
}

impl TunerSession {
    fn new(
        tuner_index: usize,
        channel: EpgChannel,
        command: String,
        mut filters: Vec<String>,
        time_limit: u64,
    ) -> Result<TunerSession, Error> {
        let mut commands = vec![command.clone()];
        commands.append(&mut filters);
        let id = TunerSessionId::new(tuner_index);
        let mut pipeline = spawn_pipeline(commands, id)?;
        let (_, output) = pipeline.take_endpoints()?;
        let broadcaster = Broadcaster::create(|ctx| {
            Broadcaster::new(id.clone(), output, time_limit, ctx)
        });

        log::info!("{}: Activated with {}", id, channel);

        Ok(TunerSession {
            id, channel, command, pipeline, broadcaster,
            subscribers: HashMap::new(), next_serial_number: 1
        })
    }

    fn is_reuseable(&self, channel: &EpgChannel) -> bool {
        self.channel.channel_type == channel.channel_type &&
            self.channel.channel == channel.channel
    }

    fn subscribe(&mut self, user: TunerUser) -> TunerSubscription {
        let serial_number = self.next_serial_number;
        self.next_serial_number += 1;

        let id = TunerSubscriptionId::new(self.id, serial_number);
        log::info!("{}: Subscribed: {}", id, user);
        self.subscribers.insert(serial_number, user);

        TunerSubscription::new(id, self.broadcaster.clone())
    }

    fn can_grab(&self, priority: TunerUserPriority) -> bool {
        self.subscribers
            .values()
            .all(|user| priority > user.priority)
    }

    fn stop_streaming(
        &mut self,
        id: TunerSubscriptionId
    ) -> Result<usize, Error> {
        if self.id != id.session_id {
            log::warn!("Session ID unmatched, {} was probably deactivated",
                       id.session_id);
            return Err(Error::SessionNotFound);
        }
        match self.subscribers.remove(&id.serial_number) {
            Some(user) => log::info!("{}: Unsubscribed: {}", id, user),
            None => log::warn!("{}: Not subscribed", id),
        }
        self.broadcaster.do_send(UnsubscribeMessage { id });
        Ok(self.subscribers.len())
    }

    fn get_mirakurun_models(
        &self
    ) -> (Option<String>, Option<u32>, Vec<MirakurunTunerUser>) {
        (
            Some(self.command.clone()),
            Some(self.pipeline.pids().as_slice().first().cloned().unwrap()),
            self.subscribers.values().map(|user| user.get_mirakurun_model()).collect(),
        )
    }
}

impl Drop for TunerSession {
    fn drop(&mut self) {
        log::info!("{}: Deactivated", self.id);
    }
}

pub struct TunerStreamStopTrigger {
    id: TunerSubscriptionId,
    recipient: Recipient<StopStreamingMessage>,
}

impl TunerStreamStopTrigger {
    pub fn new(
        id: TunerSubscriptionId,
        recipient: Recipient<StopStreamingMessage>
    ) -> Self {
        Self { id, recipient }
    }
}

impl Drop for TunerStreamStopTrigger {
    fn drop(&mut self) {
        log::debug!("{}: Closing...", self.id);
        let _ = self.recipient.do_send(StopStreamingMessage {
            id: self.id
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::*;
    use crate::command_util::Error as CommandUtilError;

    #[actix_rt::test]
    async fn test_activate_tuner() {
        let config: Arc<Config> = Arc::new(serde_yaml::from_str(r#"
            tuners:
              - name: bs
                types: [BS]
                command: >-
                  true
              - name: gr
                types: [GR]
                command: >-
                  true
        "#).unwrap());

        let mut manager = TunerManager::new(config);
        manager.load_tuners();

        let result = manager.activate_tuner(create_channel("0"), create_user(0.into()));
        assert!(result.is_ok());
        // `assert_matches!(result, Ok(subscription) => ...)` doesn't work because
        // TunerSubscription has not implemented std::fmt::Debug.
        let prev = result.unwrap();
        assert_eq!(prev.id.session_id.tuner_index, 1);

        // Reuse the tuner
        let result = manager.activate_tuner(create_channel("0"), create_user(1.into()));
        assert!(result.is_ok());
        let subscription = result.unwrap();
        assert_eq!(subscription.id.session_id, prev.id.session_id);
        assert_ne!(subscription.id.serial_number, prev.id.serial_number);

        // Lower priority user cannot grab the tuner
        let result = manager.activate_tuner(create_channel("1"), create_user(0.into()));
        assert!(result.is_err());

        // Higher priority user can grab the tuner
        let result = manager.activate_tuner(create_channel("1"), create_user(2.into()));
        assert!(result.is_ok());
        let subscription = result.unwrap();
        assert_eq!(subscription.id.session_id.tuner_index, prev.id.session_id.tuner_index);
        assert_ne!(subscription.id.session_id.session_number, prev.id.session_id.session_number);
    }

    #[actix_rt::test]
    async fn test_tuner_is_active() {
        let config = create_config("true".to_string());
        let mut tuner = Tuner::new(0, &config);

        assert!(!tuner.is_active());

        let result = tuner.activate(create_channel("1"), vec![]);
        assert!(result.is_ok());
        assert!(tuner.is_active());

        // Workaround for actix/actix/issues/372.
        //
        // See masnagam/rust-case-studies/tree/master/actix-started-in-drop for
        // details.
        tokio::task::yield_now().await;
    }

    #[actix_rt::test]
    async fn test_tuner_activate() {
        {
            let config = create_config("true".to_string());
            let mut tuner = Tuner::new(0, &config);
            let result = tuner.activate(create_channel("1"), vec![]);
            assert!(result.is_ok());
            tokio::task::yield_now().await;
        }

        {
            let config = create_config("cmd '".to_string());
            let mut tuner = Tuner::new(0, &config);
            let result = tuner.activate(create_channel("1"), vec![]);
            assert_matches!(result, Err(Error::CommandFailed(
                CommandUtilError::UnableToParse(_))));
            tokio::task::yield_now().await;
        }

        {
            let config = create_config("no-such-command".to_string());
            let mut tuner = Tuner::new(0, &config);
            let result = tuner.activate(create_channel("1"), vec![]);
            assert_matches!(result, Err(Error::CommandFailed(
                CommandUtilError::UnableToSpawn(..))));
            tokio::task::yield_now().await;
        }
    }

    #[actix_rt::test]
    async fn test_tuner_stop_streaming() {
        let config = create_config("true".to_string());
        let mut tuner = Tuner::new(1, &config);
        let result = tuner.stop_streaming(Default::default());
        assert_matches!(result, Err(Error::SessionNotFound));

        let result = tuner.activate(create_channel("1"), vec![]);
        assert!(result.is_ok());
        let subscription = tuner.subscribe(TunerUser {
            info: TunerUserInfo::Web { id: "".to_string(), agent: None },
            priority: 0.into(),
        });

        let result = tuner.stop_streaming(Default::default());
        assert_matches!(result, Err(Error::SessionNotFound));

        let result = tuner.stop_streaming(subscription.id);
        assert_matches!(result, Ok(()));

        tokio::task::yield_now().await;
    }

    #[actix_rt::test]
    async fn test_tuner_can_grab() {
        let config = create_config("true".to_string());
        let mut tuner = Tuner::new(0, &config);
        assert!(tuner.can_grab(0.into()));

        tuner.activate(create_channel("1"), vec![]).unwrap();
        tuner.subscribe(create_user(0.into()));

        assert!(!tuner.can_grab(0.into()));
        assert!(tuner.can_grab(1.into()));
        assert!(tuner.can_grab(2.into()));
        assert!(tuner.can_grab(TunerUserPriority::GRAB));

        tuner.subscribe(create_user(1.into()));

        assert!(!tuner.can_grab(0.into()));
        assert!(!tuner.can_grab(1.into()));
        assert!(tuner.can_grab(2.into()));
        assert!(tuner.can_grab(TunerUserPriority::GRAB));

        tuner.subscribe(create_user(TunerUserPriority::GRAB));

        assert!(!tuner.can_grab(0.into()));
        assert!(!tuner.can_grab(1.into()));
        assert!(!tuner.can_grab(2.into()));
        assert!(tuner.can_grab(TunerUserPriority::GRAB));

        tokio::task::yield_now().await;
    }

    #[actix_rt::test]
    async fn test_tuner_reactivate() {
        let config = create_config("true".to_string());
        let mut tuner = Tuner::new(0, &config);
        tuner.activate(create_channel("1"), vec![]).ok();

        tokio::task::yield_now().await;

        tuner.deactivate();
        let result = tuner.activate(create_channel("2"), vec![]);
        assert!(result.is_ok());

        tokio::task::yield_now().await;
    }

    fn create_config(command: String) -> TunerConfig {
        TunerConfig {
            name: String::new(),
            channel_types: vec![ChannelType::GR],
            command,
            time_limit: 10 * 1000,
            disabled: false,
            decoded: false,
        }
    }

    fn create_channel(channel: &str) -> EpgChannel {
        EpgChannel {
            name: "".to_string(),
            channel_type: ChannelType::GR,
            channel: channel.to_string(),
            extra_args: "".to_string(),
            services: vec![],
            excluded_services: vec![],
        }
    }

    fn create_user(priority: TunerUserPriority) -> TunerUser {
        TunerUser {
            info: TunerUserInfo::Job { name: "test".to_string() },
            priority
        }
    }
}
