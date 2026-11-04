#![warn(missing_docs)]
//! Esqueleto do serviço de um app do Lucerna.
//!
//! **Protocolo:** fala o protocolo 1.0 (pelo `lucerna-ipc`), tanto no socket do
//! app quanto no `shell.sock` (onde se apresenta com `client` = o id do app e
//! `app` = `"shell"`).
//!
//! Compõe o servidor do [`lucerna_ipc`] com a ativação pelo systemd (ou o socket
//! criado à mão, no desenvolvimento), a saída por ociosidade e a conexão ao
//! `shell.sock` ([`Shell`]) e os timers agendados: os do systemd do usuário
//! ([`schedule`]) ou, sem ele, os do agendador local ([`local`]).
//!
//! Com a feature `testing`, o módulo `testing` traz o `FakeShell` e o
//! `FakeScheduler` para os testes dos apps (ligue-a em `[dev-dependencies]`).

pub mod activation;
pub mod idle;
pub mod local;
pub mod schedule;
pub mod service;
pub mod shell;
#[cfg(feature = "testing")]
pub mod testing;

use std::io;
use std::path::PathBuf;

use lucerna_paths::PathsError;

/// O servidor do protocolo e os tópicos, para o app declarar o que oferece
/// sem depender do `lucerna-ipc` direto.
pub use lucerna_ipc::{ServerBuilder, Topic, TopicKind};
/// Tipos do protocolo que os apps usam com o serviço.
pub use lucerna_protocol::{BarItem, RpcError};

pub use activation::{Activation, SocketGuard};
pub use idle::{WorkGuard, WorkTracker};
#[doc(hidden)]
pub use local::render_autostart;
pub use local::{Fired, LocalScheduler, WAKE_STEP};
pub use schedule::{
    ScheduleError, Scheduler, SchedulerKind, SystemdScheduler, TimerSpec, choose_scheduler,
};
#[doc(hidden)]
pub use schedule::{calendar_utc, render_timer, timer_unit_name};
pub use service::{DEFAULT_IDLE, ServiceBuilder};
#[doc(hidden)]
pub use shell::backoff;
pub use shell::{Shell, ShellTask};

/// Erros ao subir ou rodar o serviço.
#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    /// Um caminho XDG que falta ou é inválido.
    #[error("{0}")]
    Paths(#[from] PathsError),
    /// Outro serviço já atende no socket.
    #[error("Já há um serviço atendendo em {}.", .0.display())]
    AlreadyRunning(PathBuf),
    /// O que o systemd passou não serve.
    #[error("{0}")]
    Activation(String),
    /// Erro de entrada e saída.
    #[error("{0}")]
    Io(#[from] io::Error),
}
