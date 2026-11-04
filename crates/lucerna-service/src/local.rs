//! O agendador local: os timers guardados no próprio serviço.
//!
//! Quando o systemd do usuário não serve (sem `systemctl`, sem o barramento
//! da sessão, a unidade do app não instalada), o serviço usa o
//! [`LocalScheduler`] no lugar do
//! [`SystemdScheduler`](crate::schedule::SystemdScheduler) (ver
//! [`choose_scheduler`](crate::schedule::choose_scheduler)). A interface é a
//! mesma ([`Scheduler`]).
//!
//! - Cada timer marcado segura um trabalho pendente ([`WorkTracker::hold`]):
//!   o serviço não sai por ociosidade enquanto houver algo agendado.
//! - No horário, o timer sai do agendador e o serviço recebe um [`Fired`]
//!   com o nome e o guarda do trabalho: o serviço não sai por ociosidade até
//!   o app largar o guarda. `cancel` depois do disparo é `Ok`, sem efeito.
//! - O horário é o do relógio de parede, conferido a cada no máximo
//!   [`WAKE_STEP`]: o relógio do `tokio` para durante a suspensão da
//!   máquina, e um timer vencido nela dispara até [`WAKE_STEP`] depois de a
//!   máquina voltar.
//! - Enquanto houver ao menos um timer **persistente**, o agendador mantém a
//!   entrada de início automático do XDG
//!   (`$XDG_CONFIG_HOME/autostart/lucerna-<app>.desktop`, ver
//!   [`render_autostart`]), que sobe o serviço no login; sem nenhum, a apaga.
//!   Os horários em si ficam só na memória: o app guarda o que agendou nos
//!   próprios dados e, ao subir, confere o que venceu e reagenda o resto.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock, Weak};
use std::time::Duration;

use lucerna_paths::{AppPaths, Env};
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::task::AbortHandle;

use crate::idle::{WorkGuard, WorkTracker};
use crate::schedule::{
    ScheduleError, Scheduler, TimerSpec, check_future, now_unix, remove_if_exists, validate_name,
};

/// O passo máximo entre duas conferências do relógio de parede.
pub const WAKE_STEP: Duration = Duration::from_secs(30);

/// O relógio de parede, em segundos Unix.
type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// Um timer do agendador local que disparou.
#[derive(Debug)]
pub struct Fired {
    /// O nome do timer.
    pub name: String,
    /// O trabalho pendente do timer: o serviço não sai por ociosidade até ele
    /// cair. Largue-o depois de fazer o trabalho.
    pub guard: WorkGuard,
}

/// O conteúdo da entrada de início automático do app.
///
/// Detalhe de implementação, público só para os testes; pode mudar sem aviso.
#[doc(hidden)]
pub fn render_autostart(app: &str) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName=Lucerna: {app}\n\
         Comment=Serviço do {app}, para o trabalho agendado\n\
         Exec=lucerna-{app} serve\nTerminal=false\nNoDisplay=true\n"
    )
}

/// Agenda timers dentro do próprio serviço (ver o [módulo](self)).
///
/// Clonar dá outra alça para o mesmo agendador.
#[derive(Clone)]
pub struct LocalScheduler {
    inner: Arc<Inner>,
}

struct Inner {
    paths: AppPaths,
    work: WorkTracker,
    fired: mpsc::UnboundedSender<Fired>,
    runtime: Handle,
    clock: RwLock<Clock>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    timers: BTreeMap<String, Entry>,
    /// O id do último timer marcado: um timer trocado (ou cancelado) que
    /// acorda atrasado não acha o próprio id no mapa e não dispara.
    last_id: u64,
}

struct Entry {
    spec: TimerSpec,
    id: u64,
    task: AbortHandle,
    // Só segura o trabalho; vai no `Fired` quando o timer dispara.
    guard: WorkGuard,
}

impl Drop for State {
    fn drop(&mut self) {
        for entry in self.timers.values() {
            entry.task.abort();
        }
    }
}

impl LocalScheduler {
    /// O agendador local do app `app` (valida o id). Cada timer segura um
    /// trabalho em `work`; os disparos vão para `fired` (sem receptor, caem,
    /// e o guarda junto).
    ///
    /// # Panics
    ///
    /// Fora de um runtime do `tokio`, como o `tokio::spawn`: as tarefas dos
    /// timers rodam no runtime de quem cria o agendador.
    pub fn new(
        app: &str,
        env: &Env,
        work: WorkTracker,
        fired: mpsc::UnboundedSender<Fired>,
    ) -> Result<LocalScheduler, ScheduleError> {
        let paths = AppPaths::new(app, env)?;
        Ok(LocalScheduler {
            inner: Arc::new(Inner {
                paths,
                work,
                fired,
                runtime: Handle::current(),
                clock: RwLock::new(Arc::new(now_unix)),
                state: Mutex::new(State::default()),
            }),
        })
    }

    /// Troca o relógio de parede, em segundos Unix (padrão:
    /// `SystemTime::now()`). Vale para todas as alças e para os timers já
    /// marcados (testes).
    #[doc(hidden)]
    pub fn with_clock(self, clock: Arc<dyn Fn() -> u64 + Send + Sync>) -> Self {
        *self
            .inner
            .clock
            .write()
            .unwrap_or_else(PoisonError::into_inner) = clock;
        self
    }

    /// Os timers marcados agora, pelo nome.
    pub fn timers(&self) -> BTreeMap<String, TimerSpec> {
        self.inner
            .lock()
            .timers
            .iter()
            .map(|(name, e)| (name.clone(), e.spec.clone()))
            .collect()
    }

    /// Desfaz todos os timers (e solta os guardas), para o serviço sair.
    ///
    /// A entrada de início automático fica se havia timer persistente (o
    /// login precisa dela para subir o serviço, que reagenda o que o app
    /// guardou) e é apagada se não havia (uma entrada velha de uma queda, por
    /// exemplo). O agendador continua usável.
    pub fn shutdown(&self) {
        let mut st = self.inner.lock();
        let persistent = has_persistent(&st);
        for (_, entry) in std::mem::take(&mut st.timers) {
            entry.task.abort();
        }
        if !persistent && let Ok(file) = self.inner.paths.autostart_file() {
            // Melhor esforço: o serviço está saindo.
            let _ = remove_if_exists(&file);
        }
    }
}

impl fmt::Debug for LocalScheduler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalScheduler")
            .field("app", &self.inner.paths.app())
            .field("timers", &self.timers())
            .finish_non_exhaustive()
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn now(&self) -> u64 {
        let clock = self
            .clock
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        clock()
    }

    /// Deixa a entrada de início automático de acordo com os timers: existe
    /// enquanto houver um persistente. Chamada com a trava do estado, para os
    /// arquivos seguirem a ordem das mudanças.
    fn sync_autostart(&self, st: &State) -> Result<(), ScheduleError> {
        let persistent = has_persistent(st);
        let file = match self.paths.autostart_file() {
            Ok(file) => file,
            // Sem a pasta não há entrada a apagar.
            Err(e) if persistent => return Err(e.into()),
            Err(_) => return Ok(()),
        };
        if persistent {
            write_replacing(&file, &render_autostart(self.paths.app()))?;
        } else {
            remove_if_exists(&file)?;
        }
        Ok(())
    }

    /// O timer `name` de id `id` venceu: sai do mapa e vai para o serviço.
    fn fire(&self, name: &str, id: u64) {
        let mut st = self.lock();
        // Trocado ou cancelado enquanto a tarefa acordava: não é mais ele.
        if st.timers.get(name).is_none_or(|e| e.id != id) {
            return;
        }
        let Some(entry) = st.timers.remove(name) else {
            return;
        };
        // Melhor esforço: não há a quem devolver o erro aqui.
        let _ = self.sync_autostart(&st);
        // Sem receptor, o disparo cai e o guarda junto.
        let _ = self.fired.send(Fired {
            name: entry.spec.name,
            guard: entry.guard,
        });
    }
}

fn has_persistent(st: &State) -> bool {
    st.timers.values().any(|e| e.spec.persistent)
}

/// Escreve `text` em `<file>.tmp` e o põe no lugar de `file` com `rename`
/// (quem lê nunca vê o arquivo pela metade); cria a pasta.
fn write_replacing(file: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut tmp = file.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = Path::new(&tmp);
    std::fs::write(tmp, text)
        .and_then(|()| std::fs::rename(tmp, file))
        .inspect_err(|_| {
            let _ = remove_if_exists(tmp);
        })
}

/// A tarefa de um timer: dorme em passos de no máximo [`WAKE_STEP`] até o
/// relógio de parede chegar a `at`. Segura só uma referência fraca: com o
/// agendador largado, os timers somem.
async fn wait_and_fire(inner: Weak<Inner>, name: String, at: u64, id: u64) {
    loop {
        let Some(now) = inner.upgrade().map(|i| i.now()) else {
            return;
        };
        if now >= at {
            break;
        }
        tokio::time::sleep(Duration::from_secs(at - now).min(WAKE_STEP)).await;
    }
    if let Some(inner) = inner.upgrade() {
        inner.fire(&name, id);
    }
}

impl Scheduler for LocalScheduler {
    fn schedule(&self, spec: &TimerSpec) -> Result<(), ScheduleError> {
        validate_name(&spec.name)?;
        check_future(spec.at_unix, self.inner.now())?;
        if spec.persistent {
            // Sem a pasta da entrada, recusa antes de mexer em qualquer coisa.
            self.inner.paths.autostart_file()?;
        }
        let mut st = self.inner.lock();
        if let Some(old) = st.timers.remove(&spec.name) {
            old.task.abort();
        }
        st.last_id += 1;
        let id = st.last_id;
        let task = self
            .inner
            .runtime
            .spawn(wait_and_fire(
                Arc::downgrade(&self.inner),
                spec.name.clone(),
                spec.at_unix,
                id,
            ))
            .abort_handle();
        st.timers.insert(
            spec.name.clone(),
            Entry {
                spec: spec.clone(),
                id,
                task,
                guard: self.inner.work.hold(),
            },
        );
        self.inner.sync_autostart(&st).inspect_err(|_| {
            // Desfaz o timer novo (o antigo já foi desfeito) e tenta deixar a
            // entrada de acordo com o que sobrou; o erro devolvido é o
            // original.
            if let Some(entry) = st.timers.remove(&spec.name) {
                entry.task.abort();
            }
            let _ = self.inner.sync_autostart(&st);
        })
    }

    fn cancel(&self, name: &str) -> Result<(), ScheduleError> {
        validate_name(name)?;
        let mut st = self.inner.lock();
        if let Some(entry) = st.timers.remove(name) {
            entry.task.abort();
        }
        self.inner.sync_autostart(&st)
    }
}
