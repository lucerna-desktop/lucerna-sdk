//! Lucerna Hello: o app de exemplo, um contador. É o molde que os apps copiam.
//!
//! - tópico `counter`: `{"value": n}`;
//! - tópico contínuo `ticks`: `{"tick": k, "bands": [64 × f32]}` a cada 1 ms,
//!   só com assinante (imita o espectro de um analisador de áudio);
//! - tópico `reminder`: `{"at": u64|null, "persistent": bool, "last_fired": u64|null}`;
//! - métodos `counter.add {"n"}`, `counter.reset`, `quiet.set {"on"}`,
//!   `reminder.set {"in", "persistent"?}` e `reminder.cancel`;
//! - na barra do shell, o item `counter` com o valor (some quando zera).
//!
//! O lembrete é o molde do trabalho agendado: o horário fica guardado em
//! `reminder.json`, na pasta de dados, e o agendador do serviço (o do systemd
//! ou o local) acorda o serviço na hora; ao subir, o serviço confere o que
//! venceu com ele parado.

use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lucerna_cli::clap::{Arg, ArgAction, Command, value_parser};
use lucerna_cli::{Call, CliApp, CliCommand};
use lucerna_service::{
    AppPaths, BarItem, Env, RpcError, ScheduleError, Scheduler, ServiceBuilder, Shell, TimerSpec,
    Timers, Topic,
};
use serde_json::{Value, json};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

/// Id do app.
pub const APP: &str = "hello";

/// Número de faixas do `ticks`.
const BANDS: usize = 64;

/// Período do `ticks`.
const TICK: Duration = Duration::from_millis(1);

/// Nome do timer do lembrete (unidade `lucerna-hello_reminder.timer` no systemd).
const REMINDER_TIMER: &str = "reminder";

/// Arquivo do lembrete, na pasta de dados: `{"at": u64, "persistent": bool}`.
const REMINDER_FILE: &str = "reminder.json";

/// O serviço montado e os tópicos dele.
///
/// O produtor do `ticks` vive enquanto [`Hello::producer`] viver: ao usar só o
/// `service`, mantenha o resto do `Hello` no escopo (um `let h = hello();`
/// seguido de `h.service.run()` já faz isso).
pub struct Hello {
    /// O serviço, pronto para `run` ou `run_with`.
    pub service: ServiceBuilder,
    /// O tópico `counter`.
    pub counter: Topic,
    /// O tópico contínuo `ticks`.
    pub ticks: Topic,
    /// O tópico `reminder`.
    pub reminder: Topic,
    /// A tarefa que produz o `ticks`; largá-la para a produção.
    pub producer: Producer,
}

/// A tarefa que produz o `ticks`. Abortada quando cai.
#[must_use = "o ticks para de ser produzido quando o Producer cai"]
pub struct Producer(JoinHandle<()>);

impl Drop for Producer {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// O contador e para onde cada mudança vai.
struct Counter {
    value: Mutex<i64>,
    topic: Topic,
    shell: Shell,
}

impl Counter {
    /// Troca o valor por `f(valor)`; `None` deixa como está e dá o erro de estouro.
    fn update(&self, f: impl FnOnce(i64) -> Option<i64>) -> Result<Value, RpcError> {
        // Publica sob a trava: o `seq` segue a ordem dos valores.
        let mut value = self.value.lock().unwrap_or_else(|e| e.into_inner());
        let v = f(*value).ok_or_else(|| RpcError::app(1, "O contador passou do limite."))?;
        *value = v;
        let data = json!({"value": v});
        self.topic.publish(data.clone());
        if v == 0 {
            self.shell.bar_clear("counter");
        } else {
            self.shell.bar_set(BarItem {
                id: "counter".into(),
                icon: "exposure_plus_1".into(),
                text: Some(v.to_string()),
                ends_at: None,
                panel: false,
            });
        }
        Ok(data)
    }
}

/// Agora, em segundos Unix.
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// O que o tópico `reminder` mostra.
#[derive(Clone, Copy, Default)]
struct ReminderState {
    at: Option<u64>,
    persistent: bool,
    last_fired: Option<u64>,
}

impl ReminderState {
    fn to_json(self) -> Value {
        json!({"at": self.at, "persistent": self.persistent, "last_fired": self.last_fired})
    }
}

/// O lembrete: o horário marcado, o arquivo que o guarda e o agendador.
///
/// Toda mudança roda com a trava de `state` e fora do runtime (no
/// `spawn_blocking`): o agendador pode rodar o `systemctl`, e marcar, tirar e
/// disparar não se cruzam.
struct Reminder {
    state: Mutex<ReminderState>,
    topic: Topic,
    timers: Timers,
    env: Env,
}

impl Reminder {
    /// `<pasta de dados>/reminder.json`.
    fn file(&self) -> Result<PathBuf, String> {
        AppPaths::new(APP, &self.env)
            .and_then(|p| p.data_dir())
            .map(|d| d.join(REMINDER_FILE))
            .map_err(|e| e.to_string())
    }

    /// Lê o arquivo; ausente ou ilegível = sem lembrete.
    fn read_file(&self) -> Option<(u64, bool)> {
        let bytes = fs::read(self.file().ok()?).ok()?;
        let v: Value = serde_json::from_slice(&bytes).ok()?;
        Some((v.get("at")?.as_u64()?, v.get("persistent")?.as_bool()?))
    }

    /// Grava o arquivo (por um temporário e `rename`: nunca fica pela metade).
    fn write_file(&self, at: u64, persistent: bool) -> Result<(), String> {
        let file = self.file()?;
        let write = || -> io::Result<()> {
            if let Some(dir) = file.parent() {
                fs::create_dir_all(dir)?;
            }
            let tmp = file.with_extension("json.tmp");
            fs::write(
                &tmp,
                json!({"at": at, "persistent": persistent}).to_string(),
            )?;
            fs::rename(&tmp, &file)
        };
        write().map_err(|e| e.to_string())
    }

    /// Apaga o arquivo; não é erro se ele não existir.
    fn remove_file(&self) {
        if let Ok(file) = self.file() {
            match fs::remove_file(&file) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => eprintln!("Não deu para apagar {}: {e}", file.display()),
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ReminderState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Troca o estado e publica (sob a trava: o `seq` segue a ordem).
    fn publish(&self, state: &mut ReminderState, new: ReminderState) {
        *state = new;
        self.topic.publish(new.to_json());
    }

    /// Marca o lembrete para `at`.
    fn set(&self, at: u64, persistent: bool) -> Result<Value, RpcError> {
        let mut state = self.lock();
        let spec = TimerSpec {
            name: REMINDER_TIMER.into(),
            at_unix: at,
            persistent,
        };
        self.timers.schedule(&spec).map_err(|e| match e {
            ScheduleError::InPast => {
                RpcError::app(2, "O lembrete precisa ser para daqui a 2 s ou mais.")
            }
            e => RpcError::app(3, e.to_string()),
        })?;
        if let Err(e) = self.write_file(at, persistent) {
            // Sem o arquivo, o lembrete não sobreviveria ao serviço parar.
            let _ = self.timers.cancel(REMINDER_TIMER);
            return Err(RpcError::app(3, e));
        }
        let last_fired = state.last_fired;
        self.publish(
            &mut state,
            ReminderState {
                at: Some(at),
                persistent,
                last_fired,
            },
        );
        Ok(json!({"at": at}))
    }

    /// Tira o lembrete.
    fn cancel(&self) -> Result<Value, RpcError> {
        let mut state = self.lock();
        self.timers
            .cancel(REMINDER_TIMER)
            .map_err(|e| RpcError::app(3, e.to_string()))?;
        self.remove_file();
        let last_fired = state.last_fired;
        self.publish(
            &mut state,
            ReminderState {
                last_fired,
                ..ReminderState::default()
            },
        );
        Ok(json!({}))
    }

    /// Dispara o lembrete marcado para `at`. Um app de verdade avisaria pelas
    /// notificações do sistema; o hello só publica.
    fn fire(&self, state: &mut ReminderState, at: u64) {
        // O timer já saiu do agendador local sozinho; no systemd, tira o
        // `.timer` que ficou (o `cancel` é igual nos dois).
        if let Err(e) = self.timers.cancel(REMINDER_TIMER) {
            eprintln!("Não deu para tirar o timer do lembrete: {e}");
        }
        self.remove_file();
        self.publish(
            state,
            ReminderState {
                at: None,
                persistent: false,
                last_fired: Some(at),
            },
        );
    }

    /// O timer disparou: dispara o lembrete se ele venceu. Um disparo velho
    /// (o lembrete foi tirado ou remarcado para depois) não faz nada.
    fn on_timer(&self) {
        let mut state = self.lock();
        if let Some(at) = state.at.filter(|&at| at <= now() + 1) {
            self.fire(&mut state, at);
        }
    }

    /// Ao subir: dispara o lembrete vencido com o serviço parado, ou o agenda
    /// de novo.
    fn on_start(&self) {
        let Some((at, persistent)) = self.read_file() else {
            return;
        };
        let mut state = self.lock();
        if at > now() + 1 {
            let spec = TimerSpec {
                name: REMINDER_TIMER.into(),
                at_unix: at,
                persistent,
            };
            match self.timers.schedule(&spec) {
                Ok(()) => {
                    let last_fired = state.last_fired;
                    self.publish(
                        &mut state,
                        ReminderState {
                            at: Some(at),
                            persistent,
                            last_fired,
                        },
                    );
                    return;
                }
                // Venceu entre a conta e o agendador: dispara já.
                Err(ScheduleError::InPast) => {}
                Err(e) => {
                    // O arquivo fica: a próxima subida tenta de novo.
                    eprintln!("Não deu para agendar o lembrete: {e}");
                    return;
                }
            }
        }
        self.fire(&mut state, at);
    }
}

/// Lê o campo `name` de `params` com `get`; ausente ou de outro tipo dá `-32602`.
fn param<T>(
    params: &Option<Value>,
    name: &str,
    what: &str,
    get: impl FnOnce(&Value) -> Option<T>,
) -> Result<T, RpcError> {
    params
        .as_ref()
        .and_then(|p| p.get(name))
        .and_then(get)
        .ok_or_else(|| RpcError::invalid_params(&format!("{name} precisa ser {what}")))
}

/// Roda `f` com o lembrete fora do runtime (o agendador pode bloquear).
async fn blocking<T: Send + 'static>(
    reminder: &Arc<Reminder>,
    f: impl FnOnce(&Reminder) -> T + Send + 'static,
) -> Option<T> {
    let r = reminder.clone();
    tokio::task::spawn_blocking(move || f(&r)).await.ok()
}

/// Monta o serviço com as pastas de `env` (`Env::from_process()` no binário).
///
/// Chame dentro do runtime: começa a tarefa que produz o `ticks`.
pub fn hello(env: &Env) -> Hello {
    let mut service = ServiceBuilder::new(APP, env!("CARGO_PKG_VERSION"));
    service.env(env.clone());
    let shell = service.shell();
    let timers = service.scheduler();
    let server = service.server();
    let counter = server.topic("counter", json!({"value": 0}));
    let ticks = server.stream("ticks");
    let reminder = server.topic("reminder", ReminderState::default().to_json());

    let state = Arc::new(Counter {
        value: Mutex::new(0),
        topic: counter.clone(),
        shell: shell.clone(),
    });
    let c = state.clone();
    server.method("counter.add", move |params| {
        let r = param(&params, "n", "um número inteiro", Value::as_i64)
            .and_then(|n| c.update(|v| v.checked_add(n)));
        async move { r }
    });
    let c = state;
    server.method("counter.reset", move |_| {
        let r = c.update(|_| Some(0));
        async move { r }
    });
    server.method("quiet.set", move |params| {
        let r = param(&params, "on", "true ou false", Value::as_bool).map(|on| {
            if on {
                shell.dnd_request("Lucerna Hello");
            } else {
                shell.dnd_release();
            }
            json!({})
        });
        async move { r }
    });

    let rem = Arc::new(Reminder {
        state: Mutex::new(ReminderState::default()),
        topic: reminder.clone(),
        timers,
        env: env.clone(),
    });
    let r = rem.clone();
    server.method("reminder.set", move |params| {
        let args = param(
            &params,
            "in",
            "um número inteiro de segundos",
            Value::as_u64,
        )
        .and_then(
            |secs| match params.as_ref().and_then(|p| p.get("persistent")) {
                None | Some(Value::Null) => Ok((secs, false)),
                Some(v) => v.as_bool().map(|p| (secs, p)).ok_or_else(|| {
                    RpcError::invalid_params("persistent precisa ser true ou false")
                }),
            },
        );
        let r = r.clone();
        async move {
            let (secs, persistent) = args?;
            let at = now().saturating_add(secs);
            blocking(&r, move |r| r.set(at, persistent))
                .await
                .unwrap_or_else(|| Err(RpcError::app(3, "O lembrete não foi marcado.")))
        }
    });
    let r = rem.clone();
    server.method("reminder.cancel", move |_| {
        let r = r.clone();
        async move {
            blocking(&r, Reminder::cancel)
                .await
                .unwrap_or_else(|| Err(RpcError::app(3, "O lembrete não foi tirado.")))
        }
    });
    let r = rem.clone();
    service.on_timer(move |name| {
        let r = r.clone();
        async move {
            if name == REMINDER_TIMER {
                blocking(&r, Reminder::on_timer).await;
            }
        }
    });
    service.on_start(move || async move {
        blocking(&rem, Reminder::on_start).await;
    });

    let producer = Producer(tokio::spawn(produce_ticks(ticks.clone())));
    Hello {
        service,
        counter,
        ticks,
        reminder,
        producer,
    }
}

/// Produz o `ticks` enquanto houver assinante; sem assinante, espera.
async fn produce_ticks(ticks: Topic) {
    let mut k: u64 = 0;
    loop {
        ticks.wait_subscribed().await;
        let mut every = tokio::time::interval(TICK);
        every.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // Um future só para o ciclo inteiro, em vez de um novo a cada quadro.
        let stop = ticks.wait_unsubscribed();
        tokio::pin!(stop);
        loop {
            tokio::select! {
                _ = every.tick() => {}
                _ = &mut stop => break,
            }
            k += 1;
            ticks.publish(json!({"tick": k, "bands": bands(k)}));
        }
    }
}

/// Um espectro de mentira: ondas que andam com `k`, entre 0 e 1.
fn bands(k: u64) -> Vec<f32> {
    let t = k as f32 * 0.01;
    (0..BANDS)
        .map(|i| {
            let x = i as f32 / BANDS as f32;
            0.5 + 0.5 * (t + x * 6.0).sin() * (1.0 - x)
        })
        .collect()
}

/// Texto do resultado do contador.
fn render_counter(v: &Value) -> String {
    format!("Contador: {}", v["value"])
}

/// Texto do resultado do `reminder.set`.
fn render_reminder(v: &Value) -> String {
    format!("Lembrete às {}.", v["at"])
}

/// A linha de comando do app.
pub fn cli() -> CliApp {
    CliApp::new(
        APP,
        "lucerna-hello",
        env!("CARGO_PKG_VERSION"),
        "Exemplo do Lucerna: um contador.",
    )
    .command(CliCommand {
        command: Command::new("add").about("Soma n ao contador.").arg(
            Arg::new("n")
                .value_name("n")
                .required(true)
                .value_parser(value_parser!(i64))
                .help("Quanto somar (pode ser negativo)."),
        ),
        to_call: |m| {
            let n = m.get_one::<i64>("n").ok_or("Falta o n.")?;
            Ok(Call {
                method: "counter.add".into(),
                params: Some(json!({ "n": n })),
            })
        },
        render: Some(render_counter),
    })
    .command(CliCommand {
        command: Command::new("reset").about("Zera o contador."),
        to_call: |_| {
            Ok(Call {
                method: "counter.reset".into(),
                params: None,
            })
        },
        render: Some(render_counter),
    })
    .command(CliCommand {
        command: Command::new("quiet")
            .about("Liga ou desliga o não perturbe.")
            .arg(
                Arg::new("state")
                    .value_name("on|off")
                    .required(true)
                    .value_parser(["on", "off"])
                    .help("on liga, off desliga."),
            ),
        to_call: |m| {
            let on = m.get_one::<String>("state").is_some_and(|s| s == "on");
            Ok(Call {
                method: "quiet.set".into(),
                params: Some(json!({ "on": on })),
            })
        },
        render: None,
    })
    .command(CliCommand {
        command: Command::new("remind")
            .about("Marca um lembrete para daqui a n segundos.")
            .arg(
                Arg::new("seconds")
                    .value_name("segundos")
                    .required(true)
                    .value_parser(value_parser!(u64))
                    .help("Daqui a quantos segundos."),
            )
            .arg(
                Arg::new("persistent")
                    .long("persistent")
                    .action(ArgAction::SetTrue)
                    .help("Sobrevive a reiniciar o computador."),
            ),
        to_call: |m| {
            let secs = m.get_one::<u64>("seconds").ok_or("Faltam os segundos.")?;
            let persistent = m.get_flag("persistent");
            Ok(Call {
                method: "reminder.set".into(),
                params: Some(json!({ "in": secs, "persistent": persistent })),
            })
        },
        render: Some(render_reminder),
    })
    .command(CliCommand {
        command: Command::new("forget").about("Tira o lembrete."),
        to_call: |_| {
            Ok(Call {
                method: "reminder.cancel".into(),
                params: None,
            })
        },
        render: None,
    })
}
