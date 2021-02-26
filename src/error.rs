use std::env;
use std::fmt;
use std::io;

use actix;
use failure::Fail;
use mustache;
use serde_json;
use serde_yaml;

use crate::command_util;

#[derive(Debug, Fail)]
pub enum Error {
    #[fail(display = "Streaming timed out")]
    StreamingTimedOut,
    #[fail(display = "Tuner unavailable")]
    TunerUnavailable,
    #[fail(display = "Channel not found")]
    ChannelNotFound,
    #[fail(display = "Service not found")]
    ServiceNotFound,
    #[fail(display = "Clock not synced")]
    ClockNotSynced,
    #[fail(display = "Program not found")]
    ProgramNotFound,
    #[fail(display = "Record not found")]
    RecordNotFound,
    #[fail(display = "Session not found")]
    SessionNotFound,
    #[fail(display = "Out of range")]
    OutOfRange,
    #[fail(display = "No content")]
    NoContent,
    #[fail(display = "Access denied")]
    AccessDenied,
    #[fail(display = "Command failed: {}", 0)]
    CommandFailed(command_util::Error),
    #[fail(display = "std::fmt::error: {}", 0)]
    FmtError(fmt::Error),
    #[fail(display = "std::io::error: {}", 0)]
    IoError(io::Error),
    #[fail(display = "JSON error: {}", 0)]
    JsonError(serde_json::Error),
    #[fail(display = "YAML error: {}", 0)]
    YamlError(serde_yaml::Error),
    #[fail(display = "Querystring error: {}", 0)]
    QuerystringError(serde_qs::Error),
    #[fail(display = "Mailbox error: {}", 0)]
    MailboxError(actix::MailboxError),
    #[fail(display = "Mustache error: {}", 0)]
    MustacheError(mustache::Error),
    #[fail(display = "std::env error: {}", 0)]
    EnvVarError(env::VarError),
    #[fail(display = "tokio::sync::broadcast error: {:?}", 0)]
    TokioSyncBroadcastError(tokio::sync::broadcast::RecvError),
}

impl From<command_util::Error> for Error {
    fn from(err: command_util::Error) -> Self {
        Self::CommandFailed(err)
    }
}

impl From<fmt::Error> for Error {
    fn from(err: fmt::Error) -> Self {
        Self::FmtError(err)
    }
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Self::IoError(err)
    }
}

impl From<serde_json::Error> for Error {
    fn from(err: serde_json::Error) -> Self {
        Self::JsonError(err)
    }
}

impl From<serde_yaml::Error> for Error {
    fn from(err: serde_yaml::Error) -> Self {
        Self::YamlError(err)
    }
}

impl From<serde_qs::Error> for Error {
    fn from(err: serde_qs::Error) -> Self {
        Self::QuerystringError(err)
    }
}

impl From<actix::MailboxError> for Error {
    fn from(err: actix::MailboxError) -> Self {
        Self::MailboxError(err)
    }
}

impl From<mustache::Error> for Error {
    fn from(err: mustache::Error) -> Self {
        Self::MustacheError(err)
    }
}

impl From<mustache::EncoderError> for Error {
    fn from(err: mustache::EncoderError) -> Self {
        Self::MustacheError(mustache::Error::from(err))
    }
}

impl From<env::VarError> for Error {
    fn from(err: env::VarError) -> Self {
        Self::EnvVarError(err)
    }
}

impl From<tokio::sync::broadcast::RecvError> for Error {
    fn from(err: tokio::sync::broadcast::RecvError) -> Self {
        Self::TokioSyncBroadcastError(err)
    }
}
