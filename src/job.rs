use std::env;
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Instant;

use actix::prelude::*;
use chrono::DateTime;
use cron;
use humantime;
use log;
use tokio::sync::Semaphore;

use crate::clock_synchronizer::ClockSynchronizer;
use crate::config::Config;
use crate::datetime_ext::*;
use crate::eit_feeder::*;
use crate::epg::*;
use crate::service_scanner::ServiceScanner;
use crate::tuner::*;

pub fn start(
    config: Arc<Config>,
    tuner_manager: Addr<TunerManager>,
    epg: Addr<Epg>,
    eit_feeder: Addr<EitFeeder>,
) -> Addr<JobManager> {
    JobManager::new(config, tuner_manager, epg, eit_feeder).start()
}

struct Job {
    kind: JobKind,
    semaphore: Arc<Semaphore>,
}

impl Job {
    fn new(kind: JobKind, semaphore: Arc<Semaphore>) -> Self {
        Job { kind, semaphore }
    }

    async fn perform<T, F>(self, fut: F) -> T
    where
        F: Future<Output = T>,
    {
        log::debug!("{}: acquiring semaphore...", self.kind);
        let _permit = self.semaphore.acquire().await;
        log::info!("{}: performing...", self.kind);
        let now = Instant::now();
        let results = fut.await;
        let elapsed = now.elapsed();
        log::info!("{}: Done, {} elapsed",
                   self.kind, humantime::format_duration(elapsed));
        results
    }
}

enum JobKind {
    ScanServices,
    SyncClocks,
    UpdateSchedules,
}

impl JobKind {
    fn create(self, semaphore: Arc<Semaphore>) -> Job {
        Job::new(self, semaphore)
    }
}

impl fmt::Display for JobKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use JobKind::*;
        match *self {
            ScanServices => write!(f, "scan-services"),
            SyncClocks => write!(f, "sync-clocks"),
            UpdateSchedules => write!(f, "update-schedules"),
        }
    }
}

pub struct JobManager {
    config: Arc<Config>,
    semaphore: Arc<Semaphore>,  // job concurrency
    scanning_services: bool,
    synchronizing_clocks: bool,
    updating_schedules: bool,
    tuner_manager: Addr<TunerManager>,
    epg: Addr<Epg>,
    eit_feeder: Addr<EitFeeder>,
}

impl JobManager {
    pub fn new(
        config: Arc<Config>,
        tuner_manager: Addr<TunerManager>,
        epg: Addr<Epg>,
        eit_feeder: Addr<EitFeeder>,
    ) -> Self {
        JobManager {
            config,
            semaphore: Arc::new(Semaphore::new(1)),
            scanning_services: false,
            synchronizing_clocks: false,
            updating_schedules: false,
            tuner_manager,
            epg,
            eit_feeder,
        }
    }

    fn calc_next_scheduled_datetime(&self, schedule: &str) -> DateTime<Jst> {
        cron::Schedule::from_str(schedule)
            .unwrap()
            .upcoming(Jst)
            .take(1)
            .nth(0)
            .unwrap()
    }

    fn scan_services(&mut self, ctx: &mut Context<Self>) {
        if Self::is_job_disabled_for_debug("scan-services") {
            log::debug!("scan-services: disabled for debug");
            return;
        }
        self.invoke_scan_services(ctx);
        self.schedule_scan_services(ctx);
    }

    fn invoke_scan_services(&mut self, ctx: &mut Context<Self>) {
        if self.scanning_services {
            log::warn!("scan-services: Already running, skip");
            return;
        }

        self.scanning_services = true;

        let scanner = ServiceScanner::new(
            self.config.jobs.scan_services.command.clone(),
            self.collect_enabled_channels(),
            self.tuner_manager.clone());

        let job = JobKind::ScanServices.create(self.semaphore.clone())
            .perform(scanner.scan_services());

        actix::fut::wrap_future::<_, Self>(job)
            .then(|results, act, _| {
                act.epg.do_send(UpdateServicesMessage { results });
                act.scanning_services = false;
                actix::fut::ready(())
            })
            .spawn(ctx);
    }

    fn schedule_scan_services(&self, ctx: &mut Context<Self>) {
        let datetime = self.calc_next_scheduled_datetime(
            &self.config.jobs.scan_services.schedule);
        log::info!("scan-services: Scheduled for {}", datetime);
        let interval = (datetime - Jst::now()).to_std().unwrap();
        ctx.run_later(interval, Self::scan_services);
    }

    fn sync_clocks(&mut self, ctx: &mut Context<Self>) {
        if Self::is_job_disabled_for_debug("sync-clocks") {
            log::debug!("sync-clocks: disabled for debug");
            return;
        }
        self.invoke_sync_clocks(ctx);
        self.schedule_sync_clocks(ctx);
    }

    fn invoke_sync_clocks(&mut self, ctx: &mut Context<Self>) {
        if self.synchronizing_clocks {
            log::warn!("sync-clocks: Already running, skip");
            return;
        }

        self.synchronizing_clocks = true;

        let sync = ClockSynchronizer::new(
            self.config.jobs.sync_clocks.command.clone(),
            self.collect_enabled_channels(),
            self.tuner_manager.clone());

        let job = JobKind::SyncClocks.create(self.semaphore.clone())
            .perform(sync.sync_clocks());

        actix::fut::wrap_future::<_, Self>(job)
            .then(|results, act, _| {
                act.epg.do_send(UpdateClocksMessage { results });
                act.synchronizing_clocks = false;
                actix::fut::ready(())
            })
            .spawn(ctx);
    }

    fn schedule_sync_clocks(&self, ctx: &mut Context<Self>) {
        let datetime = self.calc_next_scheduled_datetime(
            &self.config.jobs.sync_clocks.schedule);
        log::info!("sync-clocks: Scheduled for {}", datetime);
        let interval = (datetime - Jst::now()).to_std().unwrap();
        ctx.run_later(interval, Self::sync_clocks);
    }

    fn update_schedules(&mut self, ctx: &mut Context<Self>) {
        if Self::is_job_disabled_for_debug("update-schedules") {
            log::debug!("update-schedules: disabled for debug");
            return;
        }
        self.invoke_update_schedules(ctx);
        self.schedule_update_schedules(ctx);
    }

    fn invoke_update_schedules(&mut self, ctx: &mut Context<Self>) {
        if self.updating_schedules {
            log::warn!("update-schedules: Already running, skip");
            return;
        }

        self.updating_schedules = true;

        let eit_feeder = self.eit_feeder.clone();

        let job = JobKind::UpdateSchedules.create(self.semaphore.clone())
            .perform(async move {
                eit_feeder.send(FeedEitSectionsMessage).await?
            });

        actix::fut::wrap_future::<_, Self>(job)
            .then(|_, act, _| {
                act.epg.do_send(SaveSchedulesMessage);
                act.updating_schedules = false;
                actix::fut::ready(())
            })
            .spawn(ctx);
    }

    fn schedule_update_schedules(&mut self, ctx: &mut Context<Self>) {
        let datetime = self.calc_next_scheduled_datetime(
            &self.config.jobs.update_schedules.schedule);
        log::info!("update-schedules: Scheduled for {}", datetime);
        let interval = (datetime - Jst::now()).to_std().unwrap();
        ctx.run_later(interval, Self::update_schedules);
    }

    fn collect_enabled_channels(&self) -> Vec<EpgChannel> {
        self.config
            .channels
            .iter()
            .filter(|config| !config.disabled)
            .cloned()
            .map(EpgChannel::from)
            .collect()
    }

    fn is_job_disabled_for_debug(job: &str) -> bool {
        env::var("MIRAKC_DEBUG_DISABLE_JOBS")
            .ok()
            .map(|var| var.split_whitespace().position(|s| s == job))
            .flatten()
            .is_some()
    }
}

impl Actor for JobManager {
    type Context = Context<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        // It's guaranteed that no response is sent before initial jobs are invoked.
        log::debug!("Started");
        self.scan_services(ctx);
        self.sync_clocks(ctx);
        self.update_schedules(ctx);
    }

    fn stopped(&mut self, _: &mut Self::Context) {
        log::debug!("Stopped");
    }
}

// invoke scan services

struct InvokeScanServicesMessage;

impl fmt::Display for InvokeScanServicesMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "InvokeScanServices")
    }
}

impl Message for InvokeScanServicesMessage {
    type Result = ();
}

impl Handler<InvokeScanServicesMessage> for JobManager {
    type Result = ();

    fn handle(
        &mut self,
        msg: InvokeScanServicesMessage,
        ctx: &mut Self::Context,
    ) -> Self::Result {
        log::debug!("{}", msg);
        self.invoke_scan_services(ctx);
    }
}

// invoke sync clocks

struct InvokeSyncClocksMessage;

impl fmt::Display for InvokeSyncClocksMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "InvokeSyncClocks")
    }
}

impl Message for InvokeSyncClocksMessage {
    type Result = ();
}

impl Handler<InvokeSyncClocksMessage> for JobManager {
    type Result = ();

    fn handle(
        &mut self,
        msg: InvokeSyncClocksMessage,
        ctx: &mut Self::Context,
    ) -> Self::Result {
        log::debug!("{}", msg);
        self.invoke_sync_clocks(ctx);
    }
}

// invoke update schedules

struct InvokeUpdateSchedulesMessage;

impl fmt::Display for InvokeUpdateSchedulesMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "InvokeUpdateSchedules")
    }
}

impl Message for InvokeUpdateSchedulesMessage {
    type Result = ();
}

impl Handler<InvokeUpdateSchedulesMessage> for JobManager {
    type Result = ();

    fn handle(
        &mut self,
        msg: InvokeUpdateSchedulesMessage,
        ctx: &mut Self::Context,
    ) -> Self::Result {
        log::debug!("{}", msg);
        self.invoke_update_schedules(ctx);
    }
}
