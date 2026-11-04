//! O agendador do serviço: o do systemd ou o local, escolhido uma vez.
//!
//! O [`Timers`] é a alça que o app pega com
//! [`ServiceBuilder::scheduler`](crate::ServiceBuilder::scheduler). Ele só
//! escolhe o agendador quando precisa: no primeiro
//! [`schedule`](Scheduler::schedule)/[`cancel`](Scheduler::cancel) ou quando o
//! [`run`](crate::ServiceBuilder::run) sobe (o serviço escolhe ao subir). A
//! escolha vale até o fim do processo:
//!
//! - o tipo forçado com
//!   [`ServiceBuilder::scheduler_kind`](crate::ServiceBuilder::scheduler_kind),
//!   se houver; senão, [`choose_scheduler`] com o `systemctl` do `PATH`;
//! - [`SchedulerKind::Systemd`]: o [`SystemdScheduler`]; uma entrada de início
//!   automático velha do app (do agendador local, numa sessão sem systemd) é
//!   apagada, porque o systemd já sobe o serviço no login;
//! - [`SchedulerKind::Local`]: o [`LocalScheduler`], com os disparos indo para
//!   o serviço (ver
//!   [`ServiceBuilder::on_timer`](crate::ServiceBuilder::on_timer)).
//!
//! Escolher pode rodar o `systemctl` e bloqueia até ele terminar: dentro do
//! `tokio`, chame o `schedule`/`cancel` de `tokio::task::spawn_blocking` (o
//! que já vale para o [`SystemdScheduler`]).

use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use lucerna_paths::{AppPaths, Env};
use tokio::runtime::Handle;
use tokio::sync::mpsc;

use crate::idle::WorkTracker;
use crate::local::{Fired, LocalScheduler};
use crate::schedule::{
    ScheduleError, Scheduler, SchedulerKind, SystemdScheduler, TimerSpec, choose_scheduler,
    remove_if_exists,
};

/// A alça do agendador do serviço (ver o [módulo](self)).
///
/// Clonar dá outra alça para o mesmo agendador.
#[derive(Clone)]
pub struct Timers {
    inner: Arc<Inner>,
}

struct Inner {
    app: String,
    work: WorkTracker,
    fired: mpsc::UnboundedSender<Fired>,
    /// O que o `ServiceBuilder` configura; lido na hora de escolher.
    config: Mutex<Config>,
    /// Segura quem escolhe: dois `schedule` juntos não escolhem duas vezes.
    resolving: Mutex<()>,
    chosen: OnceLock<Chosen>,
}

#[derive(Default)]
pub(crate) struct Config {
    pub(crate) env: Option<Env>,
    pub(crate) kind: Option<SchedulerKind>,
}

enum Chosen {
    Systemd(SystemdScheduler),
    Local(LocalScheduler),
}

impl Timers {
    pub(crate) fn new(app: &str, work: WorkTracker, fired: mpsc::UnboundedSender<Fired>) -> Self {
        Timers {
            inner: Arc::new(Inner {
                app: app.to_string(),
                work,
                fired,
                config: Mutex::new(Config::default()),
                resolving: Mutex::new(()),
                chosen: OnceLock::new(),
            }),
        }
    }

    /// O agendador em uso; `None` antes de escolher.
    pub fn kind(&self) -> Option<SchedulerKind> {
        self.inner.chosen.get().map(|c| match c {
            Chosen::Systemd(_) => SchedulerKind::Systemd,
            Chosen::Local(_) => SchedulerKind::Local,
        })
    }

    /// Muda a configuração lida ao escolher (sem efeito depois da escolha).
    pub(crate) fn configure(&self, f: impl FnOnce(&mut Config)) {
        f(&mut self
            .inner
            .config
            .lock()
            .unwrap_or_else(PoisonError::into_inner));
    }

    /// O `Env` configurado, ou o do processo.
    pub(crate) fn env(&self) -> Env {
        self.inner
            .config
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .env
            .clone()
            .unwrap_or_else(Env::from_process)
    }

    /// O agendador local, se foi o escolhido.
    pub(crate) fn local(&self) -> Option<&LocalScheduler> {
        match self.inner.chosen.get() {
            Some(Chosen::Local(l)) => Some(l),
            _ => None,
        }
    }

    /// Escolhe o agendador, se ainda não escolheu. Um erro não fica
    /// guardado: a próxima chamada tenta de novo.
    ///
    /// Bloqueia (pode rodar o `systemctl`); precisa de um runtime do `tokio`
    /// em volta (vale o de `spawn_blocking`) para o agendador local.
    pub(crate) fn resolve(&self) -> Result<(), ScheduleError> {
        self.chosen().map(|_| ())
    }

    fn chosen(&self) -> Result<&Chosen, ScheduleError> {
        if let Some(c) = self.inner.chosen.get() {
            return Ok(c);
        }
        let _resolving = self
            .inner
            .resolving
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(c) = self.inner.chosen.get() {
            return Ok(c);
        }
        let (env, kind) = {
            let cfg = self
                .inner
                .config
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            (cfg.env.clone().unwrap_or_else(Env::from_process), cfg.kind)
        };
        let app = &self.inner.app;
        let kind = kind.unwrap_or_else(|| choose_scheduler(app, &env, Path::new("systemctl")));
        let chosen = match kind {
            SchedulerKind::Systemd => {
                let s = SystemdScheduler::new(app, &env)?;
                // Melhor esforço: o systemd já sobe o serviço no login.
                if let Ok(file) = AppPaths::new(app, &env).and_then(|p| p.autostart_file()) {
                    let _ = remove_if_exists(&file);
                }
                Chosen::Systemd(s)
            }
            SchedulerKind::Local => {
                // O `LocalScheduler::new` entra em pânico fora de um runtime;
                // aqui vira um erro.
                if Handle::try_current().is_err() {
                    return Err(ScheduleError::Io(std::io::Error::other(
                        "o agendador local precisa de um runtime do tokio",
                    )));
                }
                Chosen::Local(LocalScheduler::new(
                    app,
                    &env,
                    self.inner.work.clone(),
                    self.inner.fired.clone(),
                )?)
            }
        };
        Ok(self.inner.chosen.get_or_init(|| chosen))
    }
}

impl fmt::Debug for Timers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Timers")
            .field("app", &self.inner.app)
            .field("kind", &self.kind())
            .finish_non_exhaustive()
    }
}

impl Scheduler for Timers {
    fn schedule(&self, spec: &TimerSpec) -> Result<(), ScheduleError> {
        match self.chosen()? {
            Chosen::Systemd(s) => s.schedule(spec),
            Chosen::Local(l) => l.schedule(spec),
        }
    }

    fn cancel(&self, name: &str) -> Result<(), ScheduleError> {
        match self.chosen()? {
            Chosen::Systemd(s) => s.cancel(name),
            Chosen::Local(l) => l.cancel(name),
        }
    }
}
