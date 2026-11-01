//! Subida do serviço pelo cliente, quando ninguém atende no socket.
//!
//! O molde do spec ("Serviço sem systemd", passo 2): o cliente tenta o socket;
//! com o arquivo ausente (`NotFound`) ou um socket velho em que ninguém ouve
//! (`ConnectionRefused`), roda `<programa> serve` solto, com o stdin em
//! `/dev/null` e o stdout e o stderr no log do serviço, e tenta de novo com
//! uma espera crescente (`10 ms × 2^n`, no máximo `200 ms`) por até
//! [`SPAWN_WAIT`]. Não subiu nesse prazo: [`ClientError::NotStarted`](crate::ClientError::NotStarted)
//! com o caminho do log, onde o serviço escreveu o porquê.
//!
//! **Dois clientes juntos** sobem dois `serve`: o lock do `bind_manual` (no
//! `lucerna-service`) deixa um só criar o socket, e o outro sai sem erro. Os
//! dois clientes conectam no que ficou.
//!
//! **Por que `process_group(0)` e não `setsid`:** o serviço precisa de um
//! grupo de processos próprio para o Ctrl+C (e o `SIGHUP` do grupo) do terminal
//! de quem o subiu não chegar até ele. `setsid` pede um `pre_exec`, que é
//! `unsafe` (proibido no workspace); `process_group(0)` faz o mesmo para o
//! caso que importa sem `unsafe`. Como o stdin, o stdout e o stderr não são
//! os do terminal, o serviço não segura o pipe de quem chamou
//! (`lucerna-hello add 1 | cat` termina na hora).
//!
//! O log recomeça do zero quando passa de [`LOG_MAX`] na hora de subir. Uma
//! thread por subida faz `wait()` no filho: num cliente de vida longa (a TUI,
//! o shell), o processo que saiu é recolhido e não vira zumbi.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

/// Quanto o cliente espera o serviço subido atender no socket.
pub const SPAWN_WAIT: Duration = Duration::from_secs(2);

/// Tamanho a partir do qual o log do serviço recomeça do zero ao subir (1 MiB).
pub const LOG_MAX: u64 = 1_048_576;

/// Como subir o serviço de um app: `<program> serve`, com a saída em `log` e
/// as variáveis `envs` a mais no ambiente.
///
/// Quem monta é o chamador (a CLI, o shell): o `lucerna-ipc` não sabe as
/// regras de pasta do `lucerna-paths`.
#[derive(Clone, Debug)]
pub struct ServeCommand {
    /// O binário do app (roda com o argumento `serve`).
    pub program: PathBuf,
    /// O arquivo de log, que recebe o stdout e o stderr do serviço.
    pub log: PathBuf,
    /// Variáveis de ambiente a mais para o serviço, além das herdadas.
    pub envs: Vec<(OsString, OsString)>,
}

impl ServeCommand {
    /// `program serve`, com o log em `log` e sem variáveis a mais.
    pub fn new(program: impl Into<PathBuf>, log: impl Into<PathBuf>) -> Self {
        ServeCommand {
            program: program.into(),
            log: log.into(),
            envs: Vec::new(),
        }
    }

    /// Acrescenta a variável `key=value` ao ambiente do serviço.
    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.envs.push((key.into(), value.into()));
        self
    }

    /// Roda `program serve` solto e volta sem esperar o serviço atender.
    ///
    /// Cria a pasta do log, abre o log em modo de acréscimo (truncando se ele
    /// passa de [`LOG_MAX`]) e liga nele o stdout e o stderr; o stdin é
    /// `/dev/null` e o processo fica num grupo próprio. Uma thread recolhe o
    /// filho quando ele sai.
    pub fn spawn(&self) -> io::Result<()> {
        let log = open_log(&self.log)?;
        let mut child = Command::new(&self.program)
            .arg("serve")
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .envs(self.envs.iter().map(|(k, v)| (k, v)))
            .process_group(0)
            .spawn()?;
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }
}

/// Abre o log para acréscimo, criando a pasta; passou de [`LOG_MAX`], recomeça.
fn open_log(path: &std::path::Path) -> io::Result<File> {
    if let Some(dir) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(dir)?;
    }
    let too_big = fs::metadata(path).is_ok_and(|md| md.len() > LOG_MAX);
    let mut opts = OpenOptions::new();
    opts.create(true);
    if too_big {
        opts.write(true).truncate(true);
    } else {
        opts.append(true);
    }
    opts.open(path)
}
