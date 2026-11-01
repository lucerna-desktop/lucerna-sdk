#![warn(missing_docs)]
//! Enquadramento por linha, servidor e cliente do protocolo do Lucerna.
//!
//! **Protocolo:** fala o protocolo 1.0 (o [`lucerna_protocol::PROTOCOL_VERSION`]
//! no aperto de mão) e aceita clientes e serviços com o mesmo `MAJOR`.

pub mod client;
pub mod framing;
pub mod raw;
pub mod server;
pub mod spawn;
mod sync;
pub mod topic;

pub use client::{Client, ClientError};
pub use framing::{FrameError, LineReader, MAX_MESSAGE, write_message};
pub use server::{Server, ServerBuilder};
pub use spawn::{LOG_MAX, SPAWN_WAIT, ServeCommand};
pub use topic::{Topic, TopicKind};
