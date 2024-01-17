use super::*;

use std::convert::Infallible;
use std::pin::Pin;

use axum::response::sse::Event;
use axum::response::sse::Sse;
use futures::stream::Stream;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::events::*;

pub(super) async fn events<T, E, R, S, O>(
    State(ConfigExtractor(config)): State<ConfigExtractor>,
    State(TunerManagerExtractor(tuner_manager)): State<TunerManagerExtractor<T>>,
    State(EpgExtractor(epg)): State<EpgExtractor<E>>,
    State(RecordingManagerExtractor(recording_manager)): State<RecordingManagerExtractor<R>>,
    State(TimeshiftManagerExtractor(timeshift_manager)): State<TimeshiftManagerExtractor<S>>,
    State(OnairProgramManagerExtractor(onair_manager)): State<OnairProgramManagerExtractor<O>>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, Error>
where
    T: Clone,
    T: Call<crate::tuner::RegisterEmitter>,
    T: TriggerFactory<crate::tuner::UnregisterEmitter>,
    E: Clone,
    E: Call<crate::epg::RegisterEmitter>,
    E: TriggerFactory<crate::epg::UnregisterEmitter>,
    R: Clone,
    R: Call<crate::recording::RegisterEmitter>,
    R: TriggerFactory<crate::recording::UnregisterEmitter>,
    S: Clone,
    S: Call<crate::timeshift::RegisterEmitter>,
    S: TriggerFactory<crate::timeshift::UnregisterEmitter>,
    O: Clone,
    O: Call<crate::onair::RegisterEmitter>,
    O: TriggerFactory<crate::onair::UnregisterEmitter>,
{
    let (sender, receiver) = mpsc::channel(32);

    let feeder = EventFeeder(sender);

    let id = tuner_manager
        .call(crate::tuner::RegisterEmitter(feeder.clone().into()))
        .await?;
    let _tuner_event_unregister_trigger =
        tuner_manager.trigger(crate::tuner::UnregisterEmitter(id));

    let id = epg
        .call(crate::epg::RegisterEmitter::ProgramsUpdated(
            feeder.clone().into(),
        ))
        .await?;
    let _epg_programs_updated_unregister_trigger =
        epg.trigger(crate::epg::UnregisterEmitter::ProgramsUpdated(id));

    let _recording_started_unregister_trigger = if config.recording.is_enabled() {
        let id = recording_manager
            .call(crate::recording::RegisterEmitter::RecordingStarted(
                feeder.clone().into(),
            ))
            .await?;
        Some(recording_manager.trigger(crate::recording::UnregisterEmitter::RecordingStarted(id)))
    } else {
        None
    };

    let _recording_stopped_unregister_trigger = if config.recording.is_enabled() {
        let id = recording_manager
            .call(crate::recording::RegisterEmitter::RecordingStopped(
                feeder.clone().into(),
            ))
            .await?;
        Some(recording_manager.trigger(crate::recording::UnregisterEmitter::RecordingStopped(id)))
    } else {
        None
    };

    let _recording_failed_unregister_trigger = if config.recording.is_enabled() {
        let id = recording_manager
            .call(crate::recording::RegisterEmitter::RecordingFailed(
                feeder.clone().into(),
            ))
            .await?;
        Some(recording_manager.trigger(crate::recording::UnregisterEmitter::RecordingFailed(id)))
    } else {
        None
    };

    let _recording_rescheduled_unregister_trigger =
        if config.recording.is_enabled() {
            let id = recording_manager
                .call(crate::recording::RegisterEmitter::RecordingRescheduled(
                    feeder.clone().into(),
                ))
                .await?;
            Some(recording_manager.trigger(
                crate::recording::UnregisterEmitter::RecordingRescheduled(id),
            ))
        } else {
            None
        };

    let _timeshift_event_unregister_trigger = if config.timeshift.is_enabled() {
        let id = timeshift_manager
            .call(crate::timeshift::RegisterEmitter(feeder.clone().into()))
            .await?;
        Some(timeshift_manager.trigger(crate::timeshift::UnregisterEmitter(id)))
    } else {
        None
    };

    let _onair_program_changed_unregister_trigger = if config.has_onair_program_trackers() {
        let id = onair_manager
            .call(crate::onair::RegisterEmitter(feeder.clone().into()))
            .await?;
        Some(onair_manager.trigger(crate::onair::UnregisterEmitter(id)))
    } else {
        None
    };

    // The Sse instance will be dropped in IntoResponse::into_response().
    // So, we have to create a wrapper for the event stream in order to
    // unregister emitters.
    let sse = Sse::new(EventStreamWrapper {
        inner: ReceiverStream::new(receiver),
        _tuner_event_unregister_trigger,
        _epg_programs_updated_unregister_trigger,
        _recording_started_unregister_trigger,
        _recording_stopped_unregister_trigger,
        _recording_failed_unregister_trigger,
        _recording_rescheduled_unregister_trigger,
        _timeshift_event_unregister_trigger,
        _onair_program_changed_unregister_trigger,
    });
    Ok(sse.keep_alive(Default::default()))
}

#[derive(Clone)]
struct EventFeeder(mpsc::Sender<Result<Event, Infallible>>);

macro_rules! impl_emit {
    ($msg:path) => {
        #[async_trait]
        impl Emit<$msg> for EventFeeder {
            async fn emit(&self, msg: $msg) {
                if let Err(_) = self.0.send(Ok(msg.into())).await {
                    tracing::warn!("Client disconnected");
                }
            }
        }

        impl From<EventFeeder> for Emitter<$msg> {
            fn from(val: EventFeeder) -> Self {
                Emitter::new(val)
            }
        }
    };
}

// tuner events

impl_emit! {crate::tuner::Event}

impl From<crate::tuner::Event> for Event {
    fn from(val: crate::tuner::Event) -> Self {
        match val {
            crate::tuner::Event::StatusChanged(tuner_index) => Self::default()
                .event("tuner.status-changed")
                .json_data(TunerStatusChanged { tuner_index })
                .unwrap(),
        }
    }
}

// epg events

impl_emit! {crate::epg::ProgramsUpdated}

impl From<crate::epg::ProgramsUpdated> for Event {
    fn from(val: crate::epg::ProgramsUpdated) -> Self {
        Self::default()
            .event("epg.programs-updated")
            .json_data(EpgProgramsUpdated {
                service_id: val.service_id,
            })
            .unwrap()
    }
}

// recording events

impl_emit! {crate::recording::RecordingStarted}

impl From<crate::recording::RecordingStarted> for Event {
    fn from(val: crate::recording::RecordingStarted) -> Self {
        Self::default()
            .event("recording.started")
            .json_data(RecordingStarted {
                program_id: val.program_id,
            })
            .unwrap()
    }
}

impl_emit! {crate::recording::RecordingStopped}

impl From<crate::recording::RecordingStopped> for Event {
    fn from(val: crate::recording::RecordingStopped) -> Self {
        Self::default()
            .event("recording.stopped")
            .json_data(RecordingStopped {
                program_id: val.program_id,
            })
            .unwrap()
    }
}

impl_emit! {crate::recording::RecordingFailed}

impl From<crate::recording::RecordingFailed> for Event {
    fn from(val: crate::recording::RecordingFailed) -> Self {
        Self::default()
            .event("recording.failed")
            .json_data(RecordingFailed {
                program_id: val.program_id,
                reason: val.reason,
            })
            .unwrap()
    }
}

impl_emit! {crate::recording::RecordingRescheduled}

impl From<crate::recording::RecordingRescheduled> for Event {
    fn from(val: crate::recording::RecordingRescheduled) -> Self {
        Self::default()
            .event("recording.rescheduled")
            .json_data(RecordingRescheduled {
                program_id: val.program_id,
            })
            .unwrap()
    }
}

// timeshift events

impl_emit! {crate::timeshift::TimeshiftEvent}

impl From<crate::timeshift::TimeshiftEvent> for Event {
    fn from(val: crate::timeshift::TimeshiftEvent) -> Self {
        match val {
            crate::timeshift::TimeshiftEvent::Timeline {
                recorder,
                start_time,
                end_time,
                duration,
            } => Self::default()
                .event("timeshift.timeline")
                .json_data(TimeshiftTimeline {
                    recorder,
                    start_time,
                    end_time,
                    duration,
                })
                .unwrap(),
            crate::timeshift::TimeshiftEvent::Started { recorder } => Self::default()
                .event("timeshift.started")
                .json_data(TimeshiftStarted { recorder })
                .unwrap(),
            crate::timeshift::TimeshiftEvent::Stopped { recorder } => Self::default()
                .event("timeshift.stopped")
                .json_data(TimeshiftStopped { recorder })
                .unwrap(),
            crate::timeshift::TimeshiftEvent::RecordStarted {
                recorder,
                record_id,
            } => Self::default()
                .event("timeshift.record-started")
                .json_data(TimeshiftRecordStarted {
                    recorder,
                    record_id,
                })
                .unwrap(),
            crate::timeshift::TimeshiftEvent::RecordUpdated {
                recorder,
                record_id,
            } => Self::default()
                .event("timeshift.record-updated")
                .json_data(TimeshiftRecordUpdated {
                    recorder,
                    record_id,
                })
                .unwrap(),
            crate::timeshift::TimeshiftEvent::RecordEnded {
                recorder,
                record_id,
            } => Self::default()
                .event("timeshift.record-ended")
                .json_data(TimeshiftRecordEnded {
                    recorder,
                    record_id,
                })
                .unwrap(),
        }
    }
}

// on-air events

impl_emit! {crate::onair::OnairProgramChanged}

impl From<crate::onair::OnairProgramChanged> for Event {
    fn from(val: crate::onair::OnairProgramChanged) -> Self {
        Event::default()
            .event("onair.program-changed")
            .json_data(OnairProgramChanged {
                service_id: val.service_id,
            })
            .unwrap()
    }
}

// A wrapper type to send `UnregisterEmitter` messages when it's destroyed.

struct EventStreamWrapper<S> {
    inner: S,
    _tuner_event_unregister_trigger: Trigger<crate::tuner::UnregisterEmitter>,
    _epg_programs_updated_unregister_trigger: Trigger<crate::epg::UnregisterEmitter>,
    _recording_started_unregister_trigger: Option<Trigger<crate::recording::UnregisterEmitter>>,
    _recording_stopped_unregister_trigger: Option<Trigger<crate::recording::UnregisterEmitter>>,
    _recording_failed_unregister_trigger: Option<Trigger<crate::recording::UnregisterEmitter>>,
    _recording_rescheduled_unregister_trigger: Option<Trigger<crate::recording::UnregisterEmitter>>,
    _timeshift_event_unregister_trigger: Option<Trigger<crate::timeshift::UnregisterEmitter>>,
    _onair_program_changed_unregister_trigger: Option<Trigger<crate::onair::UnregisterEmitter>>,
}

impl<S> Stream for EventStreamWrapper<S>
where
    S: Stream + Unpin,
{
    type Item = S::Item;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}
