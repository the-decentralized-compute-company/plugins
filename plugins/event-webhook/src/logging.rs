//! Structured-enough logging for a plugin process.
//!
//! A plugin is a child process; the host does not install a tracing subscriber
//! inside it, so everything goes to stderr with a fixed prefix. Nothing in this
//! module is allowed to receive a raw webhook URL — callers redact first (see
//! [`crate::config::redact`] and [`crate::config::scrub`]).

const PREFIX: &str = "event-webhook";

pub fn info(message: impl AsRef<str>) {
    eprintln!("{PREFIX}: {}", message.as_ref());
}

pub fn warn(message: impl AsRef<str>) {
    eprintln!("{PREFIX}: warning: {}", message.as_ref());
}

pub fn error(message: impl AsRef<str>) {
    eprintln!("{PREFIX}: error: {}", message.as_ref());
}
