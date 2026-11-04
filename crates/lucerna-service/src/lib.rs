#![warn(missing_docs)]
//! Esqueleto do serviço de um app do Lucerna.
//!
//! **Protocolo:** fala o protocolo 1.0 (pelo `lucerna-ipc`), tanto no socket do
//! app quanto no `shell.sock` (onde se apresenta com `client` = o id do app e
//! `app` = `"shell"`).
//!
//! Compõe o servidor do [`lucerna_ipc`] com a ativação pelo systemd (ou o socket
//! criado à mão), a saída por ociosidade, a conexão ao `shell.sock`
//! ([`Shell`]) e os timers agendados ([`Timers`]): os do systemd do usuário
//! ([`schedule`]) ou, sem ele, os do agendador local ([`local`]).
//!
//! O serviço funciona com ou sem systemd. Quem o sobe é o systemd (pelo
//! socket) ou o cliente que não achou ninguém no socket; quem o acorda no
//! horário é o timer do systemd ou o agendador local, que chama o
//! [`ServiceBuilder::on_timer`]. Nos dois casos, o app confere o que venceu
//! com o serviço parado no [`ServiceBuilder::on_start`].
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
pub mod timers;

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
pub use timers::Timers;

/// As pastas do app e o ambiente que as define (ver [`ServiceBuilder::env`]).
pub use lucerna_paths::{AppPaths, Env};

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
