use std::sync::Arc;

use actlet::prelude::*;
use futures::StreamExt;
use reqwest_eventsource::Event;
use reqwest_eventsource::EventSource;

use crate::config::RemoteOnairProgramTrackerConfig;
use crate::epg::EpgProgram;
use crate::epg::QueryProgram;
use crate::models::MirakurunProgram;
use crate::models::MirakurunServiceId;
use crate::models::ServiceTriple;
use crate::web::WebOnairProgram;

use super::OnairProgramChanged;

pub struct RemoteTracker<E>(Arc<Inner<E>>);

impl<E> RemoteTracker<E> {
    pub fn new(
        name: String,
        config: Arc<RemoteOnairProgramTrackerConfig>,
        epg: E,
        emitter: Emitter<OnairProgramChanged>,
    ) -> Self {
        RemoteTracker(Arc::new(Inner {
            name,
            config,
            epg,
            emitter,
        }))
    }
}

// actor

#[async_trait]
impl<E> Actor for RemoteTracker<E>
where
    E: Clone + Send + Sync + 'static,
    E: Call<QueryProgram>,
{
    async fn started(&mut self, ctx: &mut Context<Self>) {
        tracing::debug!(tracker.name = self.0.name, "Started");

        let tracker = self.0.clone();
        ctx.spawn_task(async move { tracker.process_events().await });

        let onair_programs = match self.0.fetch_onair_programs().await {
            Ok(onair_programs) => onair_programs,
            Err(err) => {
                tracing::error!(
                    %err,
                    tracker.name = self.0.name,
                    "Failed to fetch on-air programs",
                );
                return;
            }
        };
        for onair_program in onair_programs.into_iter() {
            self.0.emit(onair_program).await;
        }
    }

    async fn stopped(&mut self, _ctx: &mut Context<Self>) {
        tracing::debug!(tracker.name = self.0.name, "Stopped");
    }
}

// helpers

struct Inner<E> {
    name: String,
    config: Arc<RemoteOnairProgramTrackerConfig>,
    epg: E,
    emitter: Emitter<OnairProgramChanged>,
}

impl<E> Inner<E>
where
    E: Clone + Send + Sync + 'static,
    E: Call<QueryProgram>,
{
    async fn process_events(&self) {
        let mut es = EventSource::get(self.config.events_url());
        while let Some(event) = es.next().await {
            match event {
                Ok(Event::Open) => {
                    tracing::debug!(tracker.name = self.name, "EventSource opened",);
                }
                Ok(Event::Message(message)) => {
                    if message.event != "onair.program-changed" {
                        continue;
                    }
                    tracing::debug!(tracker.name = self.name, "Message arrived",);
                    let service_id = match parse_message_data(&message.data) {
                        Ok(service_id) => service_id,
                        Err(err) => {
                            tracing::error!(
                                %err,
                                tracker.name = self.name,
                                message.data,
                                "Failed to parse JSON",
                            );
                            continue;
                        }
                    };
                    let onair_program = match self.fetch_onair_program(service_id).await {
                        Ok(onair_program) => onair_program,
                        Err(err) => {
                            tracing::error!(
                                %err,
                                tracker.name = self.name,
                                %service_id,
                                "Failed to fetch on-air program",
                            );
                            continue;
                        }
                    };
                    self.emit(onair_program).await;
                }
                Err(err) => {
                    tracing::error!(
                        %err,
                        tracker.name = self.name,
                        "Error occurs, close EventSource",
                    );
                    es.close();
                }
            }
        }
    }

    async fn emit(&self, onair_program: WebOnairProgram) {
        if !self.config.matches(onair_program.service_id) {
            return;
        }

        if onair_program.is_empty() {
            // In this case, we cannot get TSID from MirakurunProgram.
            // It's needed for creating ServiceTriple.
            tracing::debug!(
                tracker.name = self.name,
                "No program contained in WebOnairProgram",
            );
            return;
        }

        let msg = OnairProgramChanged {
            service_triple: onair_program.service_triple(),
            current: self.convert_program(onair_program.current).await,
            next: self.convert_program(onair_program.next).await,
        };
        self.emitter.emit(msg).await;
    }

    // MirakurunProgram is not compatible with EpgProgram.
    // Some of information might be lost...
    //
    // The same situation occurs when fetching on-air program information from
    // web endpoints that broadcasters provide, such as NHK's Now On Air API
    // (ver.2).
    async fn convert_program(&self, program: Option<MirakurunProgram>) -> Option<Arc<EpgProgram>> {
        let program = program?;
        let quad = (
            program.network_id,
            program.transport_stream_id,
            program.service_id,
            program.event_id,
        )
            .into();
        let mut converted = EpgProgram::new(quad);
        converted.start_at = program.start_at;
        converted.duration = program.duration;
        converted.scrambled = !program.is_free;
        converted.name = program.name;
        converted.description = program.description;
        converted.extended = program.extended;
        converted.genres = program.genres;
        // Supplement properties with information in EPG.
        if let Ok(Ok(epg)) = self.epg.call(QueryProgram::ByProgramQuad(quad)).await {
            let check: MirakurunProgram = epg.clone().into();
            if program.video != check.video {
                tracing::debug!(
                    tracker.name = self.name,
                    %program.id,
                    maybe_lost = "program.video",
                );
            }
            converted.video = epg.video;
            if program.audio != check.audio || program.audios != check.audios {
                tracing::debug!(
                    tracker.name = self.name,
                    %program.id,
                    maybe_lost = "program.audios",
                );
            }
            converted.audios = epg.audios;
            if program.series != check.series {
                tracing::debug!(
                    tracker.name = self.name,
                    %program.id,
                    maybe_lost = "program.series",
                );
            }
            converted.series = epg.series;
            if program.related_items != check.related_items {
                tracing::debug!(
                    tracker.name = self.name,
                    %program.id,
                    maybe_lost = "program.event_group",
                );
            }
            converted.event_group = epg.event_group;
        }
        Some(Arc::new(converted))
    }

    async fn fetch_onair_programs(&self) -> reqwest::Result<Vec<WebOnairProgram>> {
        reqwest::get(self.config.onair_url())
            .await?
            .json::<Vec<WebOnairProgram>>()
            .await
    }

    async fn fetch_onair_program(
        &self,
        service_id: MirakurunServiceId,
    ) -> reqwest::Result<WebOnairProgram> {
        reqwest::get(self.config.onair_url_of(service_id))
            .await?
            .json::<WebOnairProgram>()
            .await
    }
}

fn parse_message_data(data: &str) -> Result<MirakurunServiceId, serde_json::Error> {
    use crate::models::events::OnairProgramChanged;
    let event: OnairProgramChanged = serde_json::from_str(data)?;
    Ok(event.service_id)
}

impl RemoteOnairProgramTrackerConfig {
    pub fn matches(&self, service_id: MirakurunServiceId) -> bool {
        if !self.services.is_empty() {
            if !self.services.contains(&service_id) {
                return false;
            }
        }
        if !self.excluded_services.is_empty() {
            if self.excluded_services.contains(&service_id) {
                return false;
            }
        }
        true
    }
}

impl WebOnairProgram {
    fn is_empty(&self) -> bool {
        self.current.is_none() && self.next.is_none()
    }

    fn service_triple(&self) -> ServiceTriple {
        if let Some(ref program) = self.current {
            ServiceTriple::from((
                program.network_id,
                program.transport_stream_id,
                program.service_id,
            ))
        } else if let Some(ref program) = self.next {
            ServiceTriple::from((
                program.network_id,
                program.transport_stream_id,
                program.service_id,
            ))
        } else {
            unreachable!()
        }
    }
}

// <coverage:exclude>
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RemoteOnairProgramTrackerUrl;
    use maplit::hashset;

    macro_rules! config {
        ($url:expr) => {
            RemoteOnairProgramTrackerConfig {
                url: RemoteOnairProgramTrackerUrl::Mirakc($url.parse().unwrap()),
                services: hashset![],
                excluded_services: hashset![],
            }
        };
    }

    #[test]
    fn test_config_matches() {
        let mut config = config!("http://dummy/");
        assert!(config.matches((1, 1).into()));
        assert!(config.matches((1, 2).into()));

        config.services = hashset![(1, 1).into()];
        assert!(config.matches((1, 1).into()));
        assert!(!config.matches((1, 2).into()));

        config.excluded_services = hashset![(1, 1).into()];
        assert!(!config.matches((1, 1).into()));
        assert!(!config.matches((1, 2).into()));
    }
}
// </coverage:exclude>
