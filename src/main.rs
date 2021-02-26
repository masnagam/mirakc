mod airtime_tracker;
mod broadcaster;
mod chunk_stream;
mod clock_synchronizer;
mod command_util;
mod config;
mod datetime_ext;
mod eit_feeder;
mod epg;
mod error;
//mod fs_util;
mod filter;
mod job;
mod models;
mod mpeg_ts_stream;
mod service_scanner;
mod string_table;
mod timeshift;
mod tokio_snippet;
mod tracing_ext;
mod tuner;
mod web;

use clap;

use crate::error::Error;
use crate::tracing_ext::init_tracing;

#[actix_rt::main]
async fn main() -> Result<(), Error> {
    let args = clap::App::new(clap::crate_name!())
        .version(clap::crate_version!())
        .about(clap::crate_description!())
        .arg(clap::Arg::with_name("config")
             .short("c")
             .long("config")
             .takes_value(true)
             .value_name("FILE")
             .env("MIRAKC_CONFIG")
             .help("Path to a configuration file in a YAML format")
             .long_help(
                 "Path to a configuration file in a YAML format.\n\
                  \n\
                  The MIRAKC_CONFIG environment variable is used if this \
                  option is not specified.  Its value has to be an absolute \
                  path.\n\
                  \n\
                  See README.md for details of the YAML format."))
        .arg(clap::Arg::with_name("log-format")
             .long("log-format")
             .value_name("FORMAT")
             .env("MIRAKC_LOG_FORMAT")
             .takes_value(true)
             .possible_values(&["text", "json"])
             .default_value("text")
             .help("Logging format"))
        .get_matches();

    init_tracing(args.value_of("log-format").unwrap());

    let config_path = args.value_of("config").expect(
        "--config option or MIRAKC_CONFIG environment must be specified");

    let config = config::load(config_path);
    let string_table = string_table::load(&config.resource.strings_yaml);

    let tuner_manager = tuner::start(config.clone());

    let timeshift_manager = timeshift::start(config.clone(), tuner_manager.clone());

    let epg = epg::start(config.clone(), vec![
        timeshift_manager.clone().recipient(),
    ]);

    let eit_feeder = eit_feeder::start(
        config.clone(), tuner_manager.clone(), epg.clone());

    let _job_manager = job::start(
        config.clone(), tuner_manager.clone(), epg.clone(), eit_feeder.clone());

    web::serve(
        config.clone(), string_table.clone(), tuner_manager.clone(),
        epg.clone(), timeshift_manager.clone()).await?;

    Ok(())
}
