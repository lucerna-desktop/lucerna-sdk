//! Timers agendados pelo systemd do usuário, e a escolha do agendador.
//!
//! O app pede "me acorde em tal horário" com um [`TimerSpec`]; o
//! [`SystemdScheduler`] escreve um `lucerna-<app>_<nome>.timer` e o liga com
//! `systemctl --user`. Quando dispara, o timer sobe `lucerna-<app>.service`: o
//! app confere o trabalho vencido ao subir e chama [`Scheduler::cancel`] depois
//! de fazê-lo (um `OnCalendar` com data fixa não volta a disparar, mas o
//! arquivo ficaria).
//!
//! O systemd recusa ligar um timer cujo serviço não carrega, então o
//! [`SystemdScheduler`] confere que o `lucerna-<app>.service` está instalado
//! antes de escrever qualquer coisa e recusa se não estiver
//! ([`ScheduleError::ServiceNotInstalled`]). O serviço, porém, não depende
//! disso: ao subir, ele escolhe ([`choose_scheduler`]) o agendador do systemd
//! quando ele serve e, senão, o [`LocalScheduler`](crate::local::LocalScheduler),
//! que guarda os horários no próprio serviço. O app não precisa saber qual
//! está em uso.
//!
//! - Persistente: o arquivo vai para `$XDG_CONFIG_HOME/systemd/user/` com
//!   `Persistent=true` e é habilitado (`enable`), então sobrevive ao reinício e
//!   dispara ao voltar se o horário passou com a máquina desligada.
//! - Não persistente: o arquivo vai para `$XDG_RUNTIME_DIR/systemd/user/` e
//!   some quando a sessão acaba.
//!
//! As pastas saem do [`Env`] do app, mas quem as procura é o systemd, que lê o
//! `XDG_CONFIG_HOME` e o `XDG_RUNTIME_DIR` do ambiente **dele**: os dois
//! precisam concordar (é o caso normal, com as variáveis padrão da sessão).
//!
//! Trocar um timer de persistente para não persistente (ou o contrário) com o
//! mesmo nome apaga o arquivo da outra pasta; o de `$XDG_CONFIG_HOME` teria
//! prioridade sobre o de `$XDG_RUNTIME_DIR`.
//!
//! As chamadas usam [`std::process::Command`] e bloqueiam até o `systemctl`
//! terminar; dentro do `tokio`, chame de `tokio::task::spawn_blocking`.
//! O `testing::FakeScheduler` (feature `testing`) serve aos testes dos apps.

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use lucerna_paths::{Env, PathsError, validate_app_id};

/// Um horário pedido ao agendador.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimerSpec {
    /// Nome do timer dentro do app: `^[a-z0-9][a-z0-9-]{0,31}$`. Agendar de
    /// novo com o mesmo nome troca o horário.
    pub name: String,
    /// Quando disparar, em segundos Unix (UTC).
    pub at_unix: u64,
    /// Se sobrevive ao reinício da máquina.
    pub persistent: bool,
}

/// Quem agenda os timers de um app.
pub trait Scheduler: Send + Sync {
    /// Agenda (ou reagenda) o timer `spec.name` para `spec.at_unix`.
    ///
    /// O horário precisa estar ao menos 2 s no futuro (`at_unix > agora + 1`),
    /// senão [`ScheduleError::InPast`]: o app faz na hora o que já venceu. No
    /// [`SystemdScheduler`], o `lucerna-<app>.service` precisa estar
    /// instalado. Se a chamada falhar ao
    /// reagendar, o timer fica sem horário (o antigo foi desfeito): o app
    /// tenta de novo ou faz o trabalho na hora.
    fn schedule(&self, spec: &TimerSpec) -> Result<(), ScheduleError>;
    /// Tira o timer `name`; não é erro se ele não existir.
    fn cancel(&self, name: &str) -> Result<(), ScheduleError>;
}

/// Erros ao agendar ou cancelar um timer.
#[derive(Debug, thiserror::Error)]
pub enum ScheduleError {
    /// O nome não segue `^[a-z0-9][a-z0-9-]{0,31}$`.
    #[error(
        "Nome de timer inválido: \"{0}\" (use a-z, 0-9 e \"-\", sem começar por \"-\", até 32)."
    )]
    InvalidName(String),
    /// O horário pedido já passou (ou vence em menos de 2 s).
    #[error("O horário pedido já passou.")]
    InPast,
    /// O `lucerna-<app>.service` não está instalado (o argumento é a unidade).
    #[error("O serviço {0} não está instalado.")]
    ServiceNotInstalled(String),
    /// Um caminho XDG que falta ou é inválido (ou o id do app).
    #[error("{0}")]
    NoDir(#[from] PathsError),
    /// Erro de entrada e saída (inclui não achar o `systemctl`).
    #[error("{0}")]
    Io(#[from] io::Error),
    /// O `systemctl` saiu com erro.
    #[error("O systemctl {args} falhou: {stderr}")]
    Systemctl {
        /// Os argumentos passados, separados por espaço.
        args: String,
        /// O código de saída (`None` se morreu por sinal).
        status: Option<i32>,
        /// O que ele escreveu no stderr, sem espaços nas pontas.
        stderr: String,
    },
}

/// Confere o nome do timer: `^[a-z0-9][a-z0-9-]{0,31}$`.
pub(crate) fn validate_name(name: &str) -> Result<(), ScheduleError> {
    let b = name.as_bytes();
    let ok = !b.is_empty()
        && b.len() <= 32
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b[1..]
            .iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-');
    if ok {
        Ok(())
    } else {
        Err(ScheduleError::InvalidName(name.to_string()))
    }
}

/// `"AAAA-MM-DD HH:MM:SS UTC"` a partir dos segundos Unix, o formato do
/// `OnCalendar`. Usa o `civil_from_days` de Howard Hinnant.
///
/// Detalhe de implementação, público só para os testes; pode mudar sem aviso.
#[doc(hidden)]
pub fn calendar_utc(at_unix: u64) -> String {
    let days = (at_unix / 86_400) as i64;
    let secs = at_unix % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC",
        secs / 3600,
        secs / 60 % 60,
        secs % 60
    )
}

/// Dias desde 1970-01-01 → (ano, mês, dia) no calendário gregoriano.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `lucerna-<app>_<name>.timer`. O `_` não aparece no id do app nem no nome
/// do timer, então dois pares diferentes nunca dão a mesma unidade.
///
/// Detalhe de implementação, público só para os testes; pode mudar sem aviso.
#[doc(hidden)]
pub fn timer_unit_name(app: &str, name: &str) -> String {
    format!("lucerna-{app}_{name}.timer")
}

/// O conteúdo do arquivo `.timer`.
///
/// Detalhe de implementação, público só para os testes; pode mudar sem aviso.
#[doc(hidden)]
pub fn render_timer(app: &str, spec: &TimerSpec) -> String {
    let mut s = format!(
        "[Unit]\nDescription=Lucerna: {app} ({name})\n\n[Timer]\n\
         OnCalendar={cal}\nAccuracySec=1s\nUnit=lucerna-{app}.service\n",
        name = spec.name,
        cal = calendar_utc(spec.at_unix),
    );
    if spec.persistent {
        s.push_str("Persistent=true\n\n[Install]\nWantedBy=timers.target\n");
    }
    s
}

/// Agenda timers escrevendo arquivos `.timer` e chamando `systemctl --user`.
#[derive(Clone, Debug)]
pub struct SystemdScheduler {
    app: String,
    env: Env,
    systemctl: PathBuf,
}

impl SystemdScheduler {
    /// O agendador do app `app` (valida o id). As pastas só são resolvidas ao
    /// agendar.
    pub fn new(app: &str, env: &Env) -> Result<SystemdScheduler, ScheduleError> {
        validate_app_id(app)?;
        Ok(SystemdScheduler {
            app: app.to_string(),
            env: env.clone(),
            systemctl: PathBuf::from("systemctl"),
        })
    }

    /// Troca o programa chamado no lugar do `systemctl` (testes).
    pub fn with_systemctl(mut self, program: PathBuf) -> Self {
        self.systemctl = program;
        self
    }

    /// `$XDG_CONFIG_HOME/systemd/user` (ou `$HOME/.config/systemd/user`).
    fn persistent_dir(&self) -> Result<PathBuf, PathsError> {
        Ok(self.env.config_home()?.join("systemd/user"))
    }

    /// `$XDG_RUNTIME_DIR/systemd/user`.
    fn runtime_dir(&self) -> Result<PathBuf, PathsError> {
        Ok(self.env.runtime_dir()?.join("systemd/user"))
    }

    fn dir(&self, persistent: bool) -> Result<PathBuf, PathsError> {
        if persistent {
            self.persistent_dir()
        } else {
            self.runtime_dir()
        }
    }

    /// `lucerna-<app>.service`.
    fn service_unit(&self) -> String {
        format!("lucerna-{}.service", self.app)
    }

    /// Confere com `systemctl --user show` que o serviço do app carrega.
    pub(crate) fn check_service(&self) -> Result<(), ScheduleError> {
        let service = self.service_unit();
        let out = self.systemctl(&["show", "-p", "LoadState", "--value", &service])?;
        if out.trim() == "loaded" {
            Ok(())
        } else {
            Err(ScheduleError::ServiceNotInstalled(service))
        }
    }

    /// Escreve o arquivo e liga o timer; `file` já está na pasta certa.
    fn install(&self, spec: &TimerSpec, unit: &str, file: &Path) -> Result<(), ScheduleError> {
        std::fs::write(file, render_timer(&self.app, spec))?;
        self.systemctl(&["daemon-reload"])?;
        if spec.persistent {
            self.systemctl(&["enable", unit])?;
        }
        self.systemctl(&["restart", unit])?;
        Ok(())
    }

    /// Roda `systemctl --user <args>` e devolve o stdout.
    fn systemctl(&self, args: &[&str]) -> Result<String, ScheduleError> {
        let out = Command::new(&self.systemctl)
            .arg("--user")
            .args(args.iter().map(OsStr::new))
            .output()?;
        if out.status.success() {
            return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
        }
        let mut joined = String::from("--user");
        for a in args {
            joined.push(' ');
            joined.push_str(a);
        }
        Err(ScheduleError::Systemctl {
            args: joined,
            status: out.status.code(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        })
    }
}

pub(crate) fn remove_if_exists(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// O relógio de parede, em segundos Unix.
pub(crate) fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Confere que `at_unix` está ao menos 2 s depois de `now` (`at > now + 1`).
pub(crate) fn check_future(at_unix: u64, now: u64) -> Result<(), ScheduleError> {
    if at_unix <= now.saturating_add(1) {
        Err(ScheduleError::InPast)
    } else {
        Ok(())
    }
}

/// Qual agendador o serviço usa.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchedulerKind {
    /// Os timers do systemd do usuário ([`SystemdScheduler`]).
    Systemd,
    /// O agendador do próprio serviço
    /// ([`LocalScheduler`](crate::local::LocalScheduler)).
    Local,
}

/// Escolhe o agendador do app `app`: [`SchedulerKind::Systemd`] quando
/// `<systemctl> --user show -p LoadState --value lucerna-<app>.service`
/// responde `loaded`; qualquer falha (id inválido, sem `systemctl`, sem o
/// barramento da sessão, a unidade não instalada) dá
/// [`SchedulerKind::Local`]. Não escreve nada.
///
/// Bloqueia até o `systemctl` terminar; dentro do `tokio`, chame de
/// `tokio::task::spawn_blocking`.
pub fn choose_scheduler(app: &str, env: &Env, systemctl: &Path) -> SchedulerKind {
    match SystemdScheduler::new(app, env)
        .and_then(|s| s.with_systemctl(systemctl.to_path_buf()).check_service())
    {
        Ok(()) => SchedulerKind::Systemd,
        Err(_) => SchedulerKind::Local,
    }
}

impl Scheduler for SystemdScheduler {
    fn schedule(&self, spec: &TimerSpec) -> Result<(), ScheduleError> {
        validate_name(&spec.name)?;
        check_future(spec.at_unix, now_unix())?;
        let dir = self.dir(spec.persistent)?;
        self.check_service()?;
        let unit = timer_unit_name(&self.app, &spec.name);
        // A outra pasta: um arquivo velho lá venceria (o de `config`) ou
        // sobraria (o de `run`).
        if let Ok(other) = self.dir(!spec.persistent) {
            let old = other.join(&unit);
            if old.exists() {
                if !spec.persistent {
                    let _ = self.systemctl(&["disable", &unit]);
                }
                remove_if_exists(&old)?;
            }
        }
        std::fs::create_dir_all(&dir)?;
        let file = dir.join(&unit);
        self.install(spec, &unit, &file).inspect_err(|_| {
            // Melhor esforço: não deixar nada meio ligado; o erro devolvido é
            // o original.
            let _ = self.systemctl(&["disable", "--now", &unit]);
            let _ = remove_if_exists(&file);
            let _ = self.systemctl(&["daemon-reload"]);
        })
    }

    fn cancel(&self, name: &str) -> Result<(), ScheduleError> {
        validate_name(name)?;
        let unit = timer_unit_name(&self.app, name);
        // Pode não existir (ou já ter sido tirado): a falha não importa.
        let _ = self.systemctl(&["disable", "--now", &unit]);
        for dir in [self.persistent_dir(), self.runtime_dir()]
            .into_iter()
            .flatten()
        {
            remove_if_exists(&dir.join(&unit))?;
        }
        self.systemctl(&["daemon-reload"])?;
        Ok(())
    }
}
