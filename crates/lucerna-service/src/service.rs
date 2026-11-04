//! O serviço: o servidor do protocolo, a ativação, o agendador e a saída por
//! ociosidade.
//!
//! O serviço funciona com ou sem systemd:
//!
//! - **quem sobe:** o systemd, pelo socket (`lucerna-<app>.socket`), ou o
//!   cliente (a linha de comando, o shell), que cria o processo `serve` quando
//!   ninguém atende no socket e o serviço cria o socket à mão;
//! - **quem acorda:** o timer do systemd, que sobe o serviço no horário, ou o
//!   agendador local, que guarda os horários no próprio serviço e chama o
//!   [`on_timer`](ServiceBuilder::on_timer) (ver [`Timers`]);
//! - **o que venceu enquanto o serviço estava parado:** o app confere no
//!   [`on_start`](ServiceBuilder::on_start), que roda ao subir, nos dois casos.

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use lucerna_ipc::ServerBuilder;
use lucerna_paths::{AppPaths, Env, shell_socket};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::ServiceError;
use crate::activation::listener_from_env;
use crate::idle::{WorkTracker, wait_idle};
use crate::local::Fired;
use crate::schedule::SchedulerKind;
use crate::shell::Shell;
use crate::timers::Timers;

type BoxFuture = Pin<Box<dyn Future<Output = ()> + Send>>;
type OnTimer = Arc<dyn Fn(String) -> BoxFuture + Send + Sync>;
type OnStart = Box<dyn FnOnce() -> BoxFuture + Send>;

/// Ociosidade padrão: 60 s sem clientes e sem trabalho pendente.
pub const DEFAULT_IDLE: Duration = Duration::from_secs(60);

/// Variável que sobrepõe a ociosidade, em milissegundos (desenvolvimento e testes).
const IDLE_ENV: &str = "LUCERNA_IDLE_TIMEOUT_MS";

/// Monta e roda o serviço de um app.
pub struct ServiceBuilder {
    app: String,
    server: ServerBuilder,
    idle: Duration,
    work: WorkTracker,
    shell: Shell,
    timers: Timers,
    fired: mpsc::UnboundedReceiver<Fired>,
    on_timer: Option<OnTimer>,
    on_start: Option<OnStart>,
}

impl ServiceBuilder {
    /// Começa o serviço do app `app`, na versão `version`.
    pub fn new(app: &str, version: &str) -> Self {
        let work = WorkTracker::new();
        let (fired_tx, fired) = mpsc::unbounded_channel();
        ServiceBuilder {
            app: app.to_string(),
            server: ServerBuilder::new(app, version),
            idle: DEFAULT_IDLE,
            shell: Shell::new(app, work.clone()),
            timers: Timers::new(app, work.clone(), fired_tx),
            fired,
            on_timer: None,
            on_start: None,
            work,
        }
    }

    /// O ambiente do serviço: as pastas do socket, do `shell.sock` e do
    /// agendador (padrão: o do processo, lido na hora de usar).
    ///
    /// Vale para o agendador enquanto ele não escolheu (ver [`Timers`]).
    pub fn env(&mut self, env: Env) {
        self.timers.configure(|c| c.env = Some(env));
    }

    /// O agendador do serviço, para marcar e tirar timers.
    ///
    /// Escolhe entre o systemd e o agendador local no primeiro uso ou quando o
    /// [`run`](Self::run) sobe (ver [`Timers`]). Pode ser pego antes do `run`.
    pub fn scheduler(&self) -> Timers {
        self.timers.clone()
    }

    /// Força o agendador, sem perguntar ao `systemctl` (testes, ou um app que
    /// quer sempre o local).
    ///
    /// Vale enquanto o agendador não escolheu.
    pub fn scheduler_kind(&mut self, kind: SchedulerKind) {
        self.timers.configure(|c| c.kind = Some(kind));
    }

    /// Chamado com o nome de cada timer do agendador local que dispara, um de
    /// cada vez.
    ///
    /// O serviço não sai por ociosidade até o future terminar. Com o systemd
    /// escolhido, nunca é chamado: o timer do systemd sobe o serviço e o app
    /// confere o que venceu no [`on_start`](Self::on_start). Sem `on_timer`, o
    /// disparo é descartado.
    pub fn on_timer<F, Fut>(&mut self, f: F)
    where
        F: Fn(String) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.on_timer = Some(Arc::new(move |name| Box::pin(f(name))));
    }

    /// Esperado ao subir: depois de o serviço ter o socket e antes de atender
    /// (os clientes esperam na fila).
    ///
    /// É onde o app confere o trabalho vencido e reagenda o resto. Um `serve`
    /// que perde a corrida pelo socket não chega aqui.
    pub fn on_start<F, Fut>(&mut self, f: F)
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.on_start = Some(Box::new(move || Box::pin(f())));
    }

    /// O servidor do protocolo, para declarar tópicos e métodos.
    pub fn server(&mut self) -> &mut ServerBuilder {
        &mut self.server
    }

    /// Quanto tempo sem clientes e sem trabalho até o serviço sair.
    ///
    /// `LUCERNA_IDLE_TIMEOUT_MS`, se definida, vence este valor em [`run`](Self::run).
    pub fn idle_timeout(&mut self, d: Duration) {
        self.idle = d;
    }

    /// O contador de trabalho pendente do serviço.
    pub fn work(&self) -> WorkTracker {
        self.work.clone()
    }

    /// Os pedidos ao shell (barra, não perturbe, TUI, painel).
    ///
    /// Os itens da barra e o não perturbe pedido contam como trabalho pendente
    /// em [`work`](Self::work).
    /// A conexão ao `shell.sock` existe enquanto o serviço roda.
    pub fn shell(&self) -> Shell {
        self.shell.clone()
    }

    /// Roda o serviço com o [`env`](Self::env) (padrão: o do processo).
    ///
    /// Usa o socket do systemd ou cria um à mão, escolhe o agendador (ver
    /// [`scheduler`](Self::scheduler)), roda o [`on_start`](Self::on_start) e
    /// sai em `Ok(())` por ociosidade ou com `SIGTERM`/`SIGINT`.
    pub async fn run(mut self) -> Result<(), ServiceError> {
        let env = self.timers.env();
        if let Some(ms) = std::env::var(IDLE_ENV)
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
        {
            self.idle = Duration::from_millis(ms);
        }
        let paths = AppPaths::new(&self.app, &env)?;
        let (listener, activation) = listener_from_env(&paths)?;
        // Escolhe o agendador ao subir (pode rodar o `systemctl`, que
        // bloqueia). Um erro não fica guardado: o primeiro uso tenta de novo.
        let timers = self.timers.clone();
        let _ = tokio::task::spawn_blocking(move || timers.resolve()).await;
        let shell = shell_socket(&env).ok();
        // Os sinais são registrados só aqui: o `run_with` não mexe no
        // tratamento de SIGTERM/SIGINT do processo.
        let mut term = signal(SignalKind::terminate())?;
        let mut int = signal(SignalKind::interrupt())?;
        let stop = async move {
            tokio::select! {
                _ = term.recv() => {}
                _ = int.recv() => {}
            }
        };
        let r = self.serve_until(listener, shell, stop).await;
        drop(activation); // o socket à mão some só depois de parar de atender
        r
    }

    /// Roda o serviço em `listener` até a ociosidade.
    ///
    /// Não trata sinais (isso é do [`run`](Self::run)) e só escolhe o
    /// agendador no primeiro uso; para parar antes, largue o future. Com `shell_socket`, mantém a conexão ao shell (ver
    /// [`shell`](Self::shell)) até sair.
    pub async fn run_with(
        self,
        listener: tokio::net::UnixListener,
        shell_socket: Option<PathBuf>,
    ) -> Result<(), ServiceError> {
        self.serve_until(listener, shell_socket, std::future::pending())
            .await
    }

    async fn serve_until(
        mut self,
        listener: tokio::net::UnixListener,
        shell_socket: Option<PathBuf>,
        stop: impl Future<Output = ()>,
    ) -> Result<(), ServiceError> {
        // Na saída (ou com o future largado), o agendador local desfaz os
        // timers e acerta a entrada de início automático.
        let _shutdown = ShutdownLocal(self.timers.clone());
        // A conexão ao shell fecha quando o serviço para (o guarda cai aqui).
        let _shell_task = shell_socket.map(|p| self.shell.spawn(p));
        tokio::pin!(stop);
        if let Some(on_start) = self.on_start.take() {
            tokio::select! {
                () = on_start() => {}
                () = &mut stop => return Ok(()),
            }
        }
        let _fired_task = AbortOnDrop(tokio::spawn(handle_fired(self.fired, self.on_timer.take())));
        let server = self.server.build();
        let idle = wait_idle(server.connections(), self.work.watch(), self.idle);
        tokio::select! {
            // `biased`: um cliente que já está na fila do `accept` é aceito (e
            // contado) antes de a ociosidade ser conferida no mesmo despertar.
            biased;
            r = server.serve(listener) => r.map_err(ServiceError::Io),
            _ = idle => Ok(()),
            () = &mut stop => Ok(()),
        }
    }
}

/// Lê os disparos do agendador local e chama o `on_timer`, um de cada vez; o
/// guarda de cada um só cai depois.
async fn handle_fired(mut fired: mpsc::UnboundedReceiver<Fired>, on_timer: Option<OnTimer>) {
    while let Some(Fired { name, guard }) = fired.recv().await {
        if let Some(f) = &on_timer {
            f(name).await;
        }
        drop(guard);
    }
}

/// Aborta a tarefa quando é largado.
struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Chama o `shutdown` do agendador local, se foi o escolhido, quando é
/// largado.
struct ShutdownLocal(Timers);

impl Drop for ShutdownLocal {
    fn drop(&mut self) {
        if let Some(local) = self.0.local() {
            local.shutdown();
        }
    }
}
