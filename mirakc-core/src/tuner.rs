use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use actlet::prelude::*;

use crate::broadcaster::*;
use crate::command_util::spawn_pipeline;
use crate::command_util::CommandPipeline;
use crate::config::Config;
use crate::config::FilterConfig;
use crate::config::OnairProgramTrackerConfig;
use crate::config::TunerConfig;
use crate::epg::EpgChannel;
use crate::error::Error;
use crate::models::*;
use crate::mpeg_ts_stream::MpegTsStream;

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
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let session_number = COUNTER.fetch_add(1, Ordering::Relaxed);
        TunerSessionId {
            tuner_index,
            session_number,
        }
    }
}

impl fmt::Display for TunerSessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.tuner_index, self.session_number)
    }
}

#[derive(Clone, Copy, PartialEq)]
#[cfg_attr(test, derive(Debug, Default))]
pub struct TunerSubscriptionId {
    session_id: TunerSessionId,
    serial_number: u32,
}

impl TunerSubscriptionId {
    pub fn new(session_id: TunerSessionId, serial_number: u32) -> Self {
        Self {
            session_id,
            serial_number,
        }
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
    event_emitters: EmitterRegistry<Event>,
}

struct TunerSubscription {
    id: TunerSubscriptionId,
    broadcaster: Address<Broadcaster>,
    max_stuck_time: std::time::Duration,
    decoded: bool,
}

impl TunerSubscription {
    fn new(
        id: TunerSubscriptionId,
        broadcaster: Address<Broadcaster>,
        max_stuck_time: std::time::Duration,
    ) -> Self {
        Self {
            id,
            broadcaster,
            max_stuck_time,
            decoded: false,
        }
    }
}

impl TunerManager {
    pub fn new(config: Arc<Config>) -> Self {
        TunerManager {
            config,
            tuners: Vec::new(),
            event_emitters: Default::default(),
        }
    }

    fn load_tuners(&mut self) {
        let tuners: Vec<Tuner> = self
            .config
            .tuners
            .iter()
            .filter(|config| !config.disabled)
            .enumerate()
            .map(|(i, config)| {
                let mut tuner = Tuner::new(i, config);
                if let Some(name) = self
                    .config
                    .onair_program_trackers
                    .iter()
                    .filter_map(|(name, config)| match config {
                        OnairProgramTrackerConfig::Local(ref config) => Some((name, config)),
                        _ => None,
                    })
                    .find_map(|(name, tracker)| {
                        if tracker.uses.tuner == config.name {
                            Some(name)
                        } else {
                            None
                        }
                    })
                {
                    let user = TunerUserInfo::OnairProgramTracker(name.clone());
                    tuner.set_exclusive_user(user);
                } else if let Some((name, timeshift)) = self
                    .config
                    .timeshift
                    .recorders
                    .iter()
                    .find_map(|(name, timeshift)| {
                        if timeshift.uses.tuner == config.name {
                            Some((name, timeshift))
                        } else {
                            None
                        }
                    })
                {
                    let user = TunerUserInfo::TimeshiftRecorder(name.to_string());
                    tuner.set_channel_user(
                        user,
                        timeshift.uses.channel_type,
                        timeshift.uses.channel.clone(),
                    );
                }
                tuner
            })
            .collect();
        tracing::info!(tuners.len = tuners.len(), "Loaded tuners");
        self.tuners = tuners;
    }

    async fn activate_tuner<C>(
        &mut self,
        channel: &EpgChannel,
        user: &TunerUser,
        stream_id: &Option<TunerSubscriptionId>,
        ctx: &C,
    ) -> Result<TunerSubscription, Error>
    where
        C: Spawn,
    {
        if let Some(stream_id) = stream_id {
            let tuner = &mut self.tuners[stream_id.session_id.tuner_index];
            if tuner.is_subscribed(stream_id) {
                tracing::debug!(tuner.index, %channel, %user.info, stream.id = %stream_id, "Reuse specified tuner");
                self.event_emitters
                    .emit(Event::StatusChanged(tuner.index))
                    .await;
                return Ok(tuner.subscribe(user));
            }
            tracing::error!(tuner.index, %channel, %user.info, stream.id = %stream_id, "Specified tuner is unavailable");
            return Err(Error::TunerUnavailable);
        }

        let found = self
            .tuners
            .iter_mut()
            .find(|tuner| tuner.is_reserved_for(user));
        if let Some(tuner) = found {
            tracing::debug!(tuner.index, %channel, %user.info, "Use reserved tuner");
            if !tuner.is_active() {
                let filters =
                    Self::make_filter_commands(tuner, channel, &self.config.filters.tuner_filter)?;
                tuner.activate(channel, filters, ctx).await?;
            }
            self.event_emitters
                .emit(Event::StatusChanged(tuner.index))
                .await;
            return Ok(tuner.subscribe(user));
        }

        let found = self
            .tuners
            .iter_mut()
            .find(|tuner| tuner.is_reuseable(channel));
        if let Some(tuner) = found {
            tracing::debug!(tuner.index, %channel, %user.info, "Reuse active tuner");
            self.event_emitters
                .emit(Event::StatusChanged(tuner.index))
                .await;
            return Ok(tuner.subscribe(user));
        }

        let found = self
            .tuners
            .iter_mut()
            .find(|tuner| tuner.is_available_for(channel));
        if let Some(tuner) = found {
            tracing::debug!(tuner.index, %channel, %user.info, "Use tuner");
            let filters =
                Self::make_filter_commands(tuner, channel, &self.config.filters.tuner_filter)?;
            tuner.activate(channel, filters, ctx).await?;
            self.event_emitters
                .emit(Event::StatusChanged(tuner.index))
                .await;
            return Ok(tuner.subscribe(user));
        }

        // No available tuner at this point.
        // Grab one of lowest priority tuner.
        let found = self
            .tuners
            .iter_mut()
            .filter(|tuner| tuner.is_supported_type(channel))
            .filter(|tuner| tuner.can_grab(user.priority))
            .min_by(|a, b| a.priority().cmp(&b.priority()));
        if let Some(tuner) = found {
            tracing::debug!(tuner.index, %channel, %user.info, %user.priority, "Grab tuner");
            let filters =
                Self::make_filter_commands(tuner, channel, &self.config.filters.tuner_filter)?;
            tuner.deactivate();
            self.event_emitters
                .emit(Event::StatusChanged(tuner.index))
                .await;
            tuner.activate(channel, filters, ctx).await?;
            return Ok(tuner.subscribe(user));
        }

        tracing::warn!(%channel, %user.info, %user.priority, "No tuner available");
        Err(Error::TunerUnavailable)
    }

    fn deactivate_tuner(&mut self, id: TunerSubscriptionId) {
        self.tuners[id.session_id.tuner_index].deactivate();
    }

    async fn stop_streaming(
        &mut self,
        id: TunerSubscriptionId,
    ) -> Result<Option<TunerUser>, Error> {
        let tuner = &mut self.tuners[id.session_id.tuner_index];
        self.event_emitters
            .emit(Event::StatusChanged(tuner.index))
            .await;
        tuner.stop_streaming(id).await
    }

    fn make_filter_commands(
        tuner: &Tuner,
        channel: &EpgChannel,
        filter: &FilterConfig,
    ) -> Result<Vec<String>, Error> {
        if filter.command.is_empty() {
            return Ok(vec![]);
        }
        let filter = match Self::make_filter_command(tuner, channel, &filter.command) {
            Ok(filter) => filter,
            Err(err) => {
                tracing::error!(tuner.index, %channel, "Failed to render tuner-filter");
                return Err(err);
            }
        };
        if filter.trim().is_empty() {
            tracing::warn!(tuner.index, %channel, "Empty tuner-filter");
            Ok(vec![])
        } else {
            Ok(vec![filter])
        }
    }

    // Template variables regarding TunerUser are not specified
    // because a tuner filter is shared between users.
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

#[async_trait]
impl Actor for TunerManager {
    async fn started(&mut self, _ctx: &mut Context<Self>) {
        // It's guaranteed that no response is sent before tuners are loaded.
        tracing::debug!("Started");
        self.load_tuners();
    }

    async fn stopped(&mut self, _ctx: &mut Context<Self>) {
        for tuner in self.tuners.iter_mut() {
            tuner.deactivate();
        }
        tracing::debug!("Stopped");
    }
}

// register emitter

#[derive(Message)]
#[reply(usize)]
pub struct RegisterEmitter(pub Emitter<Event>);

#[async_trait]
impl Handler<RegisterEmitter> for TunerManager {
    async fn handle(
        &mut self,
        msg: RegisterEmitter,
        ctx: &mut Context<Self>,
    ) -> <RegisterEmitter as Message>::Reply {
        let id = self.event_emitters.register(msg.0.clone());
        tracing::debug!(msg.name = "RegisterEmitter", id);
        if id != 0 {
            // Sending many messages in the message handler may cause a deadlock
            // when the number of messages to be sent is larger than the capacity
            // of the emitter's channel.  See the issue #705 for example.
            ctx.set_post_process(RegisterEmitterPostProcess(msg.0));
        }
        id
    }
}

#[derive(Message)]
struct RegisterEmitterPostProcess(Emitter<Event>);

#[async_trait]
impl Handler<RegisterEmitterPostProcess> for TunerManager {
    async fn handle(&mut self, msg: RegisterEmitterPostProcess, _ctx: &mut Context<Self>) {
        let emitter = msg.0;
        for tuner in self.tuners.iter().filter(|tuner| tuner.is_active()) {
            emitter.emit(Event::StatusChanged(tuner.index)).await;
        }
    }
}

// unregister emitter

#[derive(Message)]
pub struct UnregisterEmitter(pub usize);

#[async_trait]
impl Handler<UnregisterEmitter> for TunerManager {
    async fn handle(&mut self, msg: UnregisterEmitter, _ctx: &mut Context<Self>) {
        tracing::debug!(msg.name = "UnregisterEmitter", id = msg.0);
        self.event_emitters.unregister(msg.0);
    }
}

// query tuners

#[derive(Message)]
#[reply(Vec<MirakurunTuner>)]
pub struct QueryTuners;

#[async_trait]
impl Handler<QueryTuners> for TunerManager {
    async fn handle(
        &mut self,
        _msg: QueryTuners,
        _ctx: &mut Context<Self>,
    ) -> <QueryTuners as Message>::Reply {
        tracing::debug!(msg.name = "QueryTuners");
        self.tuners
            .iter()
            .map(|tuner| tuner.get_mirakurun_model())
            .collect()
    }
}

// query tuner

#[derive(Message)]
#[reply(Result<MirakurunTuner, Error>)]
pub struct QueryTuner(pub usize);

#[async_trait]
impl Handler<QueryTuner> for TunerManager {
    async fn handle(
        &mut self,
        msg: QueryTuner,
        _ctx: &mut Context<Self>,
    ) -> <QueryTuner as Message>::Reply {
        tracing::debug!(msg.name = "QueryTuner", msg.index = msg.0);
        self.tuners
            .get(msg.0)
            .map(|tuner| tuner.get_mirakurun_model())
            .ok_or(Error::TunerNotFound)
    }
}

// start streaming

#[derive(Message)]
#[reply(Result<TunerStream, Error>)]
pub struct StartStreaming {
    pub channel: EpgChannel,
    pub user: TunerUser,
    pub stream_id: Option<TunerSubscriptionId>,
}

#[async_trait]
impl Handler<StartStreaming> for TunerManager {
    async fn handle(
        &mut self,
        msg: StartStreaming,
        ctx: &mut Context<Self>,
    ) -> <StartStreaming as Message>::Reply {
        tracing::debug!(msg.name = "StartStreaming", %msg.channel, %msg.user.info, %msg.user.priority);

        let subscription = self
            .activate_tuner(&msg.channel, &msg.user, &msg.stream_id, ctx)
            .await?;

        let result = subscription
            .broadcaster
            .call(Subscribe {
                id: subscription.id,
                max_stuck_time: subscription.max_stuck_time,
            })
            .await;
        match result {
            Ok(stream) => {
                if msg.user.is_short_term_user() {
                    // Suppress noisy logs caused by short-term jobs such as on-air program trackers.
                    tracing::debug!(channel = %msg.channel, user.info = %msg.user.info, user.priority = %msg.user.priority, stream.id = %subscription.id, "Streaming started");
                } else {
                    tracing::info!(channel = %msg.channel, user.info = %msg.user.info, user.priority = %msg.user.priority, stream.id = %subscription.id, "Streaming started");
                }
                let stream = MpegTsStream::new(subscription.id, stream);
                let stream = if subscription.decoded {
                    stream.decoded()
                } else {
                    stream
                };
                Ok(stream)
            }
            Err(err) => {
                tracing::error!(%err, %subscription.id, "Broadcaster may have stopped");
                self.deactivate_tuner(subscription.id);
                Err(err.into())
            }
        }
    }
}

// stop streaming

#[derive(Message)]
pub struct StopStreaming {
    pub id: TunerSubscriptionId,
}

#[async_trait]
impl Handler<StopStreaming> for TunerManager {
    async fn handle(&mut self, msg: StopStreaming, _ctx: &mut Context<Self>) {
        tracing::debug!(msg.name = "StopStreaming", %msg.id);
        match self.stop_streaming(msg.id).await {
            Ok(Some(user)) if user.is_short_term_user() => {
                tracing::debug!(stream.id = %msg.id, "Streaming stopped");
            }
            _ => {
                tracing::info!(stream.id = %msg.id, "Streaming stopped");
            }
        }
    }
}

// event

#[derive(Clone, Message)]
pub enum Event {
    StatusChanged(usize),
}

// tuner

struct Tuner {
    index: usize,
    name: String,
    channel_types: Vec<ChannelType>,
    command: String,
    time_limit: u64,
    decoded: bool,
    reserved_for: Option<TunerUserInfo>,
    restriction: Restriction,
    activity: TunerActivity,
}

enum Restriction {
    None,
    Channel(ChannelType, String),
    Exclusive,
}

impl Tuner {
    fn new(index: usize, config: &TunerConfig) -> Self {
        Tuner {
            index,
            name: config.name.clone(),
            channel_types: config.channel_types.clone(),
            command: config.command.clone(),
            time_limit: config.time_limit,
            decoded: config.decoded,
            reserved_for: None,
            restriction: Restriction::None,
            activity: TunerActivity::Inactive,
        }
    }

    fn set_channel_user(
        &mut self,
        user: TunerUserInfo,
        channel_type: ChannelType,
        channel: String,
    ) {
        assert!(self.reserved_for.is_none());
        self.reserved_for = Some(user);
        self.restriction = Restriction::Channel(channel_type, channel);
    }

    fn set_exclusive_user(&mut self, user: TunerUserInfo) {
        assert!(self.reserved_for.is_none());
        self.reserved_for = Some(user);
        self.restriction = Restriction::Exclusive;
    }

    fn is_subscribed(&self, id: &TunerSubscriptionId) -> bool {
        self.activity.is_subscribed(id)
    }

    fn is_active(&self) -> bool {
        self.activity.is_active()
    }

    fn is_available(&self) -> bool {
        self.activity.is_inactive()
    }

    fn is_supported_type(&self, channel: &EpgChannel) -> bool {
        if let Restriction::Channel(ch_type, ref ch) = self.restriction {
            debug_assert_eq!(channel.channel_type, ch_type);
            debug_assert_eq!(channel.channel, ch.as_str());
        }
        self.channel_types.contains(&channel.channel_type)
    }

    fn is_available_for(&self, channel: &EpgChannel) -> bool {
        if let Restriction::Channel(ch_type, ref ch) = self.restriction {
            debug_assert_eq!(channel.channel_type, ch_type);
            debug_assert_eq!(channel.channel, ch.as_str());
        }
        match self.restriction {
            Restriction::Exclusive => false,
            _ => self.is_available() && self.is_supported_type(channel),
        }
    }

    fn is_reuseable(&self, channel: &EpgChannel) -> bool {
        if let Restriction::Channel(ch_type, ref ch) = self.restriction {
            debug_assert_eq!(channel.channel_type, ch_type);
            debug_assert_eq!(channel.channel, ch.as_str());
        }
        match self.restriction {
            Restriction::Exclusive => false,
            _ => self.activity.is_reuseable(channel),
        }
    }

    fn is_reserved_for(&self, user: &TunerUser) -> bool {
        match self.reserved_for {
            Some(ref user_info) => user_info.eq(&user.info),
            None => false,
        }
    }

    fn can_grab(&self, priority: TunerUserPriority) -> bool {
        match self.restriction {
            Restriction::None => priority.is_grab() || self.activity.can_grab(priority),
            _ => false,
        }
    }

    fn priority(&self) -> Option<TunerPriority> {
        self.activity.priority()
    }

    async fn activate<C>(
        &mut self,
        channel: &EpgChannel,
        filters: Vec<String>,
        ctx: &C,
    ) -> Result<(), Error>
    where
        C: Spawn,
    {
        let command = match self.make_command(channel) {
            Ok(command) => command,
            Err(err) => {
                tracing::error!(%err, tuner.index = self.index, %channel, "Failed to render the tuner command");
                return Err(err);
            }
        };
        self.activity
            .activate(self.index, channel, command, filters, self.time_limit, ctx)
            .await
    }

    fn deactivate(&mut self) {
        self.activity.deactivate();
    }

    fn subscribe(&mut self, user: &TunerUser) -> TunerSubscription {
        let mut subscription = self.activity.subscribe(user);
        subscription.decoded = self.decoded;
        subscription
    }

    async fn stop_streaming(
        &mut self,
        id: TunerSubscriptionId,
    ) -> Result<Option<TunerUser>, Error> {
        self.activity.unsubscript(id).await
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

#[derive(Debug, Eq, PartialEq)]
struct TunerPriority {
    highest_user_priority: TunerUserPriority,
    num_users: usize,
}

impl Ord for TunerPriority {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        if self.highest_user_priority != other.highest_user_priority {
            return self.highest_user_priority.cmp(&other.highest_user_priority);
        }
        self.num_users.cmp(&other.num_users)
    }
}

impl PartialOrd for TunerPriority {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

// activity

enum TunerActivity {
    Inactive,
    Active(TunerSession),
}

impl TunerActivity {
    async fn activate<C>(
        &mut self,
        tuner_index: usize,
        channel: &EpgChannel,
        command: String,
        filters: Vec<String>,
        time_limit: u64,
        ctx: &C,
    ) -> Result<(), Error>
    where
        C: Spawn,
    {
        match self {
            Self::Inactive => {
                let session =
                    TunerSession::new(tuner_index, channel, command, filters, time_limit, ctx)
                        .await?;
                *self = Self::Active(session);
                Ok(())
            }
            Self::Active(_) => panic!("Must be deactivated before activating"),
        }
    }

    fn deactivate(&mut self) {
        *self = Self::Inactive;
    }

    fn is_subscribed(&self, id: &TunerSubscriptionId) -> bool {
        match self {
            Self::Inactive => false,
            Self::Active(session) => session.is_subscribed(id),
        }
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

    fn subscribe(&mut self, user: &TunerUser) -> TunerSubscription {
        match self {
            Self::Inactive => panic!("Must be activated before subscribing"),
            Self::Active(session) => session.subscribe(user),
        }
    }

    async fn unsubscript(&mut self, id: TunerSubscriptionId) -> Result<Option<TunerUser>, Error> {
        match self {
            Self::Inactive => {
                tracing::warn!(subscription.id = %id, "Inactive, probably already deactivated");
                Err(Error::SessionNotFound)
            }
            Self::Active(session) => {
                let result = session.unsubscribe(id).await;
                if session.has_no_subscriber() {
                    self.deactivate();
                }
                result
            }
        }
    }

    fn can_grab(&self, priority: TunerUserPriority) -> bool {
        match self {
            Self::Inactive => true,
            Self::Active(session) => session.can_grab(priority),
        }
    }

    fn priority(&self) -> Option<TunerPriority> {
        match self {
            Self::Inactive => None,
            Self::Active(session) => session.priority(),
        }
    }

    fn get_mirakurun_models(&self) -> (Option<String>, Option<u32>, Vec<MirakurunTunerUser>) {
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
    // Used for closing the tuner in order to take over the right to use it.
    pipeline: CommandPipeline<TunerSessionId>,
    broadcaster: Address<Broadcaster>,
    subscribers: HashMap<u32, TunerUser>,
    next_serial_number: u32,
}

impl TunerSession {
    async fn new<C>(
        tuner_index: usize,
        channel: &EpgChannel,
        command: String,
        mut filters: Vec<String>,
        time_limit: u64,
        ctx: &C,
    ) -> Result<TunerSession, Error>
    where
        C: Spawn,
    {
        let id = TunerSessionId::new(tuner_index);
        let mut commands = vec![command];
        commands.append(&mut filters);
        let mut pipeline = match spawn_pipeline(commands, id, "tuner") {
            Ok(pipeline) => pipeline,
            Err(err) => {
                tracing::error!(%err, session.id = %id, %channel, "Failed to spawn a tuner pipeline");
                return Err(err.into());
            }
        };
        let (_, output) = pipeline.take_endpoints();
        let broadcaster = ctx.spawn_actor(Broadcaster::new(id, time_limit)).await;
        broadcaster.emit(BindStream(output)).await;
        tracing::debug!(session.id = %id, %channel, "Activated");

        Ok(TunerSession {
            id,
            channel: channel.clone(),
            pipeline,
            broadcaster,
            subscribers: HashMap::new(),
            next_serial_number: 1,
        })
    }

    fn is_subscribed(&self, id: &TunerSubscriptionId) -> bool {
        self.subscribers.contains_key(&id.serial_number)
    }

    fn is_reuseable(&self, channel: &EpgChannel) -> bool {
        self.channel.channel_type == channel.channel_type && self.channel.channel == channel.channel
    }

    fn subscribe(&mut self, user: &TunerUser) -> TunerSubscription {
        let serial_number = self.next_serial_number;
        self.next_serial_number += 1;

        let id = TunerSubscriptionId::new(self.id, serial_number);
        tracing::debug!(subscription.id = %id, %user.info, "Subscribed");
        self.subscribers.insert(serial_number, user.clone());

        TunerSubscription::new(id, self.broadcaster.clone(), user.max_stuck_time())
    }

    fn can_grab(&self, priority: TunerUserPriority) -> bool {
        self.subscribers
            .values()
            .all(|user| priority > user.priority)
    }

    async fn unsubscribe(&mut self, id: TunerSubscriptionId) -> Result<Option<TunerUser>, Error> {
        if self.id != id.session_id {
            tracing::warn!(subscription.id = %id, "Session ID unmatched, probably already deactivated");
            return Err(Error::SessionNotFound);
        }
        let user = self.subscribers.remove(&id.serial_number);
        match user {
            Some(ref user) => tracing::debug!(subscription.id = %id, %user.info, "Unsubscribed"),
            None => tracing::warn!(subscription.id = %id, "Not subscribed"),
        }
        self.broadcaster.emit(Unsubscribe { id }).await;
        Ok(user)
    }

    fn has_no_subscriber(&self) -> bool {
        self.subscribers.is_empty()
    }

    fn priority(&self) -> Option<TunerPriority> {
        self.subscribers
            .values()
            .map(|user| user.priority)
            .max()
            .map(|prio| TunerPriority {
                highest_user_priority: prio,
                num_users: self.subscribers.len(),
            })
    }

    fn get_mirakurun_models(&self) -> (Option<String>, Option<u32>, Vec<MirakurunTunerUser>) {
        let command = self.pipeline.get_command(0).map(|s| s.to_string());
        let pids = self.pipeline.pids().iter().cloned().next().flatten();
        let users = self
            .subscribers
            .values()
            .map(|user| user.get_mirakurun_model())
            .collect();
        (command, pids, users)
    }
}

impl Drop for TunerSession {
    fn drop(&mut self) {
        tracing::debug!(session.id = %self.id, "Deactivated");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command_util::Error as CommandUtilError;
    use assert_matches::assert_matches;
    use test_log::test;

    #[test]
    fn test_make_filter_command() {
        let config = create_config("true".to_string());
        let tuner = Tuner::new(0, &config);
        let channel = channel_gr!("name", "channel");

        assert_matches!(TunerManager::make_filter_command(&tuner, &channel, "{{tuner_index}}"), Ok(cmd) => {
            assert_eq!(cmd, "0");
        });
        assert_matches!(TunerManager::make_filter_command(&tuner, &channel, "{{tuner_name}}"), Ok(cmd) => {
            assert_eq!(cmd, "tuner");
        });
        assert_matches!(TunerManager::make_filter_command(&tuner, &channel, "{{channel_name}}"), Ok(cmd) => {
            assert_eq!(cmd, "name");
        });
        assert_matches!(TunerManager::make_filter_command(&tuner, &channel, "{{channel_type}}"), Ok(cmd) => {
            assert_eq!(cmd, "GR");
        });
        assert_matches!(TunerManager::make_filter_command(&tuner, &channel, "{{channel}}"), Ok(cmd) => {
            assert_eq!(cmd, "channel");
        });
    }

    #[test(tokio::test)]
    async fn test_start_streaming() {
        let system = System::new();

        {
            let config: Arc<Config> = Arc::new(
                serde_yml::from_str(
                    r#"
                tuners:
                  - name: bs
                    types: [BS]
                    command: >-
                      sleep 1
                  - name: gr
                    types: [GR]
                    command: >-
                      sleep 1
                  - name: tracker
                    types: [GR]
                    command: >-
                      sleep 1
                onair-program-trackers:
                  tracker:
                    local:
                      channel-types: [GR]
                      uses:
                        tuner: tracker
                "#,
                )
                .unwrap(),
            );

            let manager = system.spawn_actor(TunerManager::new(config)).await;

            let result = manager
                .call(StartStreaming {
                    channel: create_channel("0"),
                    user: create_user(0.into()),
                    stream_id: None,
                })
                .await;
            let stream1 = assert_matches!(result, Ok(Ok(stream)) => {
                assert_eq!(stream.id().session_id.tuner_index, 1);
                stream
            });

            // Reuse the tuner
            let result = manager
                .call(StartStreaming {
                    channel: create_channel("0"),
                    user: create_user(1.into()),
                    stream_id: None,
                })
                .await;
            assert_matches!(result, Ok(Ok(stream)) => {
                assert_eq!(stream.id().session_id, stream1.id().session_id);
                assert_ne!(stream.id(), stream1.id());
            });

            // Lower and same priority user cannot grab the tuner
            let result = manager
                .call(StartStreaming {
                    channel: create_channel("1"),
                    user: create_user(1.into()),
                    stream_id: None,
                })
                .await;
            assert_matches!(result, Ok(Err(Error::TunerUnavailable)));

            // Higher priority user can grab the tuner
            let result = manager
                .call(StartStreaming {
                    channel: create_channel("1"),
                    user: create_user(2.into()),
                    stream_id: None,
                })
                .await;
            assert_matches!(result, Ok(Ok(stream)) => {
                assert_eq!(stream.id().session_id.tuner_index, 1);
                assert_ne!(stream.id().session_id, stream1.id().session_id);
            });

            // Dedicated tuner
            let result = manager
                .call(StartStreaming {
                    channel: create_channel("0"),
                    user: TunerUser {
                        info: TunerUserInfo::OnairProgramTracker("tracker".to_string()),
                        priority: 0.into(),
                    },
                    stream_id: None,
                })
                .await;
            assert_matches!(result, Ok(Ok(stream)) => {
                assert_eq!(stream.id().session_id.tuner_index, 2);
                assert_ne!(stream.id().session_id, stream1.id().session_id);
            });
        }
        system.stop();
    }

    #[test(tokio::test)]
    async fn test_start_streaming_grab_lowest_priority_tuner() {
        let config: Arc<Config> = Arc::new(
            serde_yml::from_str(
                r#"
                tuners:
                  - name: gr1
                    types: [GR]
                    command: >-
                      sleep 1
                  - name: gr2
                    types: [GR]
                    command: >-
                      sleep 1
                "#,
            )
            .unwrap(),
        );

        let system = System::new();
        {
            let manager = system.spawn_actor(TunerManager::new(config.clone())).await;

            let result = manager
                .call(StartStreaming {
                    channel: create_channel("1"),
                    user: create_user(1.into()),
                    stream_id: None,
                })
                .await;
            let _stream0 = assert_matches!(result, Ok(Ok(stream)) => {
                assert_eq!(stream.id().session_id.tuner_index, 0);
                stream
            });

            let result = manager
                .call(StartStreaming {
                    channel: create_channel("0"),
                    user: create_user(0.into()),
                    stream_id: None,
                })
                .await;
            let _stream1 = assert_matches!(result, Ok(Ok(stream)) => {
                assert_eq!(stream.id().session_id.tuner_index, 1);
                stream
            });

            // Grab the lowest priority tuner.
            let result = manager
                .call(StartStreaming {
                    channel: create_channel("2"),
                    user: create_user(2.into()),
                    stream_id: None,
                })
                .await;
            assert_matches!(result, Ok(Ok(stream)) => {
                assert_eq!(stream.id().session_id.tuner_index, 1);
            });
        }
        system.stop();

        let system = System::new();
        {
            let manager = system.spawn_actor(TunerManager::new(config.clone())).await;

            let result = manager
                .call(StartStreaming {
                    channel: create_channel("0"),
                    user: create_user(0.into()),
                    stream_id: None,
                })
                .await;
            let _stream0 = assert_matches!(result, Ok(Ok(stream)) => {
                assert_eq!(stream.id().session_id.tuner_index, 0);
                stream
            });

            let result = manager
                .call(StartStreaming {
                    channel: create_channel("1"),
                    user: create_user(1.into()),
                    stream_id: None,
                })
                .await;
            let _stream1 = assert_matches!(result, Ok(Ok(stream)) => {
                assert_eq!(stream.id().session_id.tuner_index, 1);
                stream
            });

            // Grab the lowest priority tuner.
            let result = manager
                .call(StartStreaming {
                    channel: create_channel("2"),
                    user: create_user(2.into()),
                    stream_id: None,
                })
                .await;
            assert_matches!(result, Ok(Ok(stream)) => {
                assert_eq!(stream.id().session_id.tuner_index, 0);
            });
        }
        system.stop();

        // same priority
        let system = System::new();
        {
            let manager = system.spawn_actor(TunerManager::new(config.clone())).await;

            let result = manager
                .call(StartStreaming {
                    channel: create_channel("0"),
                    user: create_user(0.into()),
                    stream_id: None,
                })
                .await;
            let _stream0 = assert_matches!(result, Ok(Ok(stream)) => {
                assert_eq!(stream.id().session_id.tuner_index, 0);
                stream
            });

            let result = manager
                .call(StartStreaming {
                    channel: create_channel("1"),
                    user: create_user(0.into()),
                    stream_id: None,
                })
                .await;
            let _stream1 = assert_matches!(result, Ok(Ok(stream)) => {
                assert_eq!(stream.id().session_id.tuner_index, 1);
                stream
            });

            let result = manager
                .call(StartStreaming {
                    channel: create_channel("0"),
                    user: create_user(0.into()),
                    stream_id: None,
                })
                .await;
            let _stream2 = assert_matches!(result, Ok(Ok(stream)) => {
                assert_eq!(stream.id().session_id.tuner_index, 0);
                stream
            });

            // Grab a tuner with fewest users.
            let result = manager
                .call(StartStreaming {
                    channel: create_channel("2"),
                    user: create_user(2.into()),
                    stream_id: None,
                })
                .await;
            assert_matches!(result, Ok(Ok(stream)) => {
                assert_eq!(stream.id().session_id.tuner_index, 1);
            });
        }
        system.stop();

        // same priority and same number of users
        let system = System::new();
        {
            let manager = system.spawn_actor(TunerManager::new(config.clone())).await;

            let result = manager
                .call(StartStreaming {
                    channel: create_channel("0"),
                    user: create_user(0.into()),
                    stream_id: None,
                })
                .await;
            let _stream0 = assert_matches!(result, Ok(Ok(stream)) => {
                assert_eq!(stream.id().session_id.tuner_index, 0);
                stream
            });

            let result = manager
                .call(StartStreaming {
                    channel: create_channel("1"),
                    user: create_user(0.into()),
                    stream_id: None,
                })
                .await;
            let _stream1 = assert_matches!(result, Ok(Ok(stream)) => {
                assert_eq!(stream.id().session_id.tuner_index, 1);
                stream
            });

            // Grab a tuner with the lowest index.
            let result = manager
                .call(StartStreaming {
                    channel: create_channel("2"),
                    user: create_user(2.into()),
                    stream_id: None,
                })
                .await;
            assert_matches!(result, Ok(Ok(stream)) => {
                assert_eq!(stream.id().session_id.tuner_index, 0);
            });
        }
        system.stop();
    }

    #[test(tokio::test)]
    async fn test_tuner_is_subscribed() {
        let system = System::new();
        {
            let config = create_config("true".to_string());
            let mut tuner = Tuner::new(0, &config);

            let dummy_id = TunerSubscriptionId::new(TunerSessionId::new(0), 1);

            assert!(!tuner.is_subscribed(&dummy_id));

            let result = tuner.activate(&create_channel("1"), vec![], &system).await;
            assert!(result.is_ok());
            assert!(!tuner.is_subscribed(&dummy_id));

            let subscription = tuner.subscribe(&TunerUser {
                info: TunerUserInfo::Web {
                    id: "".to_string(),
                    agent: None,
                },
                priority: 0.into(),
            });
            assert!(tuner.is_subscribed(&subscription.id));

            let result = tuner.stop_streaming(subscription.id).await;
            assert!(result.is_ok());
            assert!(!tuner.is_subscribed(&subscription.id));
        }
        system.stop();
    }

    #[test(tokio::test)]
    async fn test_tuner_is_active() {
        let system = System::new();
        {
            let config = create_config("true".to_string());
            let mut tuner = Tuner::new(0, &config);

            assert!(!tuner.is_active());

            let result = tuner.activate(&create_channel("1"), vec![], &system).await;
            assert!(result.is_ok());
            assert!(tuner.is_active());
        }
        system.stop();
    }

    #[test(tokio::test)]
    async fn test_tuner_activate() {
        let system = System::new();
        {
            let config = create_config("true".to_string());
            let mut tuner = Tuner::new(0, &config);
            let result = tuner.activate(&create_channel("1"), vec![], &system).await;
            assert!(result.is_ok());
            tokio::task::yield_now().await;
        }

        {
            let config = create_config("cmd '".to_string());
            let mut tuner = Tuner::new(0, &config);
            let result = tuner.activate(&create_channel("1"), vec![], &system).await;
            assert_matches!(
                result,
                Err(Error::CommandFailed(CommandUtilError::UnableToParse(_)))
            );
            tokio::task::yield_now().await;
        }

        {
            let config = create_config("no-such-command".to_string());
            let mut tuner = Tuner::new(0, &config);
            let result = tuner.activate(&create_channel("1"), vec![], &system).await;
            assert_matches!(
                result,
                Err(Error::CommandFailed(CommandUtilError::UnableToSpawn(..)))
            );
            tokio::task::yield_now().await;
        }
        system.stop();
    }

    #[test(tokio::test)]
    async fn test_tuner_stop_streaming() {
        let system = System::new();
        {
            let config = create_config("true".to_string());
            let mut tuner = Tuner::new(1, &config);
            let result = tuner.stop_streaming(Default::default()).await;
            assert_matches!(result, Err(Error::SessionNotFound));

            let result = tuner.activate(&create_channel("1"), vec![], &system).await;
            assert!(result.is_ok());
            let subscription = tuner.subscribe(&TunerUser {
                info: TunerUserInfo::Web {
                    id: "".to_string(),
                    agent: None,
                },
                priority: 0.into(),
            });

            let result = tuner.stop_streaming(Default::default()).await;
            assert_matches!(result, Err(Error::SessionNotFound));

            let result = tuner.stop_streaming(subscription.id).await;
            assert_matches!(result, Ok(Some(user)) => {
                assert_matches!(user.info, TunerUserInfo::Web { .. });
            });

            tokio::task::yield_now().await;
        }
        system.stop();
    }

    #[test(tokio::test)]
    async fn test_tuner_can_grab() {
        let system = System::new();
        {
            let config = create_config("true".to_string());
            let mut tuner = Tuner::new(0, &config);
            assert!(tuner.can_grab(0.into()));

            tuner
                .activate(&create_channel("1"), vec![], &system)
                .await
                .unwrap();
            tuner.subscribe(&create_user(0.into()));

            assert!(!tuner.can_grab(0.into()));
            assert!(tuner.can_grab(1.into()));
            assert!(tuner.can_grab(2.into()));
            assert!(tuner.can_grab(TunerUserPriority::GRAB));

            tuner.subscribe(&create_user(1.into()));

            assert!(!tuner.can_grab(0.into()));
            assert!(!tuner.can_grab(1.into()));
            assert!(tuner.can_grab(2.into()));
            assert!(tuner.can_grab(TunerUserPriority::GRAB));

            tuner.subscribe(&create_user(TunerUserPriority::GRAB));

            assert!(!tuner.can_grab(0.into()));
            assert!(!tuner.can_grab(1.into()));
            assert!(!tuner.can_grab(2.into()));
            assert!(tuner.can_grab(TunerUserPriority::GRAB));

            tokio::task::yield_now().await;
        }
        system.stop();
    }

    #[test(tokio::test)]
    async fn test_tuner_priority() {
        let system = System::new();
        {
            let config = create_config("true".to_string());
            let mut tuner = Tuner::new(0, &config);
            assert_matches!(tuner.priority(), None);

            tuner
                .activate(&create_channel("1"), vec![], &system)
                .await
                .unwrap();

            tuner.subscribe(&create_user(0.into()));
            assert_matches!(tuner.priority(), Some(prio) => {
                assert_eq!(prio.highest_user_priority, 0.into());
                assert_eq!(prio.num_users, 1);
            });

            tuner.subscribe(&create_user(10.into()));
            assert_matches!(tuner.priority(), Some(prio) => {
                assert_eq!(prio.highest_user_priority, 10.into());
                assert_eq!(prio.num_users, 2);
            });

            tuner.subscribe(&create_user(5.into()));
            assert_matches!(tuner.priority(), Some(prio) => {
                assert_eq!(prio.highest_user_priority, 10.into());
                assert_eq!(prio.num_users, 3);
            });
        }
        system.stop();
    }

    #[test(tokio::test)]
    async fn test_tuner_reactivate() {
        let system = System::new();
        {
            let config = create_config("true".to_string());
            let mut tuner = Tuner::new(0, &config);
            tuner
                .activate(&create_channel("1"), vec![], &system)
                .await
                .ok();

            tokio::task::yield_now().await;

            tuner.deactivate();
            let result = tuner.activate(&create_channel("2"), vec![], &system).await;
            assert!(result.is_ok());

            tokio::task::yield_now().await;
        }
        system.stop();
    }

    fn create_config(command: String) -> TunerConfig {
        TunerConfig {
            name: "tuner".to_string(),
            channel_types: vec![ChannelType::GR],
            command,
            time_limit: 10 * 1000,
            disabled: false,
            decoded: false,
        }
    }

    fn create_channel(channel: &str) -> EpgChannel {
        channel_gr!("", channel)
    }

    fn create_user(priority: TunerUserPriority) -> TunerUser {
        tuner_user!(priority, job; "test")
    }
}

#[cfg(test)]
pub(crate) mod stub {
    use super::*;
    use bytes::Bytes;

    #[derive(Clone, Default)]
    pub(crate) struct TunerManagerStub {
        expected_priority: Option<TunerUserPriority>,
    }

    impl TunerManagerStub {
        pub fn new(test_config: Arc<HashMap<&'static str, String>>) -> Self {
            TunerManagerStub {
                expected_priority: test_config
                    .get("tuner_user_priority")
                    .map(|json| serde_json::from_str(json).unwrap()),
            }
        }
    }

    #[async_trait]
    impl Call<RegisterEmitter> for TunerManagerStub {
        async fn call(
            &self,
            _msg: RegisterEmitter,
        ) -> actlet::Result<<RegisterEmitter as Message>::Reply> {
            Ok(0)
        }
    }

    stub_impl_fire! {TunerManagerStub, UnregisterEmitter}

    #[async_trait]
    impl Call<QueryTuners> for TunerManagerStub {
        async fn call(&self, _msg: QueryTuners) -> actlet::Result<<QueryTuners as Message>::Reply> {
            Ok(vec![])
        }
    }

    #[async_trait]
    impl Call<QueryTuner> for TunerManagerStub {
        async fn call(&self, msg: QueryTuner) -> actlet::Result<<QueryTuner as Message>::Reply> {
            match msg.0 {
                0 => Ok(Err(Error::TunerNotFound)),
                _ => Ok(Ok(tuner!(msg.0))),
            }
        }
    }

    #[async_trait]
    impl Call<StartStreaming> for TunerManagerStub {
        async fn call(
            &self,
            msg: StartStreaming,
        ) -> actlet::Result<<StartStreaming as Message>::Reply> {
            if let Some(expected_priority) = self.expected_priority {
                assert_eq!(msg.user.priority, expected_priority);
            }
            if msg.channel.channel == "ch" {
                let (tx, stream) = BroadcasterStream::new_for_test();
                let _ = tx.try_send(Bytes::from("hi"));
                Ok(Ok(MpegTsStream::new(
                    TunerSubscriptionId::default(),
                    stream,
                )))
            } else {
                let (_, stream) = BroadcasterStream::new_for_test();
                Ok(Ok(MpegTsStream::new(
                    TunerSubscriptionId::default(),
                    stream,
                )))
            }
        }
    }

    stub_impl_fire! {TunerManagerStub, StopStreaming}
}
