//! Como o serviço recebe o socket: do systemd (`LISTEN_FDS`) ou criado à mão.
//!
//! O socket criado à mão serve ao desenvolvimento e também é o caminho do
//! serviço subido pelo cliente (sem systemd, ou com a unidade não instalada).
//! Dois `serve` subindo juntos (dois clientes que não acharam ninguém) não se
//! atrapalham: o lock `<app>.lock` deixa um só criar o socket, e o outro
//! recebe [`ServiceError::AlreadyRunning`].

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use lucerna_paths::{AppPaths, create_private_dir};

use crate::ServiceError;

/// O primeiro fd que o systemd passa (`SD_LISTEN_FDS_START`).
const LISTEN_FDS_START: i32 = 3;

/// Se o fd do systemd já virou um listener neste processo.
static TAKEN: AtomicBool = AtomicBool::new(false);

/// Quantos fds o systemd passou a este processo.
///
/// Devolve `0` se falta alguma variável, se ela não é um número ou se
/// `LISTEN_PID` não é `own_pid` (as variáveis eram de outro processo).
pub fn listen_fds(listen_pid: Option<&str>, listen_fds: Option<&str>, own_pid: u32) -> usize {
    let (Some(pid), Some(fds)) = (listen_pid, listen_fds) else {
        return 0;
    };
    match (pid.parse::<u32>(), fds.parse::<usize>()) {
        (Ok(pid), Ok(fds)) if pid == own_pid => fds,
        _ => 0,
    }
}

/// Transforma o fd recebido num [`UnixListener`] não bloqueante.
///
/// Reabre o fd com `try_clone()` para ter `CLOEXEC` (o fd 3 do systemd vem sem
/// ele e vazaria para os processos filhos) e fecha o original.
///
/// Limitação: sem `libc` não conferimos `SO_TYPE` nem `SO_ACCEPTCONN`. O
/// `local_addr()` garante que é um socket Unix, e a unidade `.socket`
/// (`ListenStream=`) garante um socket de fluxo em escuta; um fd errado falha
/// no primeiro `accept` com o erro do sistema.
pub fn listener_from_fd(fd: OwnedFd) -> io::Result<UnixListener> {
    let fd = fd.try_clone()?; // o original fecha ao sair daqui
    let listener = UnixListener::from(fd);
    listener.local_addr()?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

/// De onde veio o socket do serviço.
#[derive(Debug)]
pub enum Activation {
    /// Passado pelo systemd; o arquivo é dele.
    Systemd,
    /// Criado à mão; o arquivo some quando o guarda cai.
    Manual(SocketGuard),
}

/// Apaga o arquivo do socket criado à mão quando é largado.
///
/// Só apaga se o arquivo ainda é o que criamos (mesmo dispositivo e inode): se
/// outro serviço tomou o caminho depois, o socket dele fica. Segura também o
/// lock `<app>.lock` ao lado do socket, que só é solto depois de apagar.
#[derive(Debug)]
pub struct SocketGuard {
    path: PathBuf,
    id: (u64, u64),
    _lock: File,
}

impl SocketGuard {
    /// O caminho do socket.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        if let Ok(md) = fs::symlink_metadata(&self.path)
            && (md.dev(), md.ino()) == self.id
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Cria o socket à mão em `socket`, com a pasta em modo `0700`.
///
/// Primeiro pega o lock `<app>.lock` na mesma pasta (com outro processo
/// segurando, [`ServiceError::AlreadyRunning`]), para que dois serviços
/// subindo juntos não apaguem o socket um do outro. Se o arquivo já existe e
/// alguém atende nele, devolve [`ServiceError::AlreadyRunning`] sem tocar nele.
/// Se é um socket velho (o `connect` é recusado), apaga e cria de novo. Um
/// arquivo que não é socket nunca é apagado.
pub fn bind_manual(socket: &Path) -> Result<(UnixListener, SocketGuard), ServiceError> {
    let running = || ServiceError::AlreadyRunning(socket.to_path_buf());
    if let Some(dir) = socket.parent().filter(|p| !p.as_os_str().is_empty()) {
        create_private_dir(dir)?;
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(socket.with_extension("lock"))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Err(running()),
        Err(TryLockError::Error(e)) => return Err(e.into()),
    }
    match fs::symlink_metadata(socket) {
        Ok(md) if md.file_type().is_socket() => match UnixStream::connect(socket) {
            Ok(_) => return Err(running()),
            Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => fs::remove_file(socket)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        },
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} existe e não é um socket.", socket.display()),
            )
            .into());
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let listener = match UnixListener::bind(socket) {
        Ok(l) => l,
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => return Err(running()),
        Err(e) => return Err(e.into()),
    };
    listener.set_nonblocking(true)?;
    let md = fs::symlink_metadata(socket)?;
    let guard = SocketGuard {
        path: socket.to_path_buf(),
        id: (md.dev(), md.ino()),
        _lock: lock,
    };
    Ok((listener, guard))
}

/// O socket do serviço: o do systemd, se `LISTEN_PID`/`LISTEN_FDS` são deste
/// processo, senão um criado à mão em `paths.socket()`.
///
/// O fd 3 passa a ser do listener devolvido; uma segunda chamada com o
/// socket do systemd dá [`ServiceError::Activation`].
pub fn listener_from_env(
    paths: &AppPaths,
) -> Result<(tokio::net::UnixListener, Activation), ServiceError> {
    let pid = std::env::var("LISTEN_PID").ok();
    let fds = std::env::var("LISTEN_FDS").ok();
    listener_from_vars(paths, pid.as_deref(), fds.as_deref())
}

fn listener_from_vars(
    paths: &AppPaths,
    pid: Option<&str>,
    fds: Option<&str>,
) -> Result<(tokio::net::UnixListener, Activation), ServiceError> {
    match listen_fds(pid, fds, std::process::id()) {
        0 => {
            let (listener, guard) = bind_manual(&paths.socket()?)?;
            let listener = tokio::net::UnixListener::from_std(listener)?;
            Ok((listener, Activation::Manual(guard)))
        }
        1 => {
            if TAKEN.swap(true, Ordering::SeqCst) {
                return Err(ServiceError::Activation(
                    "O socket do systemd já foi usado neste processo.".into(),
                ));
            }
            // SAFETY: com `LISTEN_PID` igual ao nosso pid e `LISTEN_FDS=1`, o
            // systemd nos passou o fd 3 aberto. O `TAKEN` acima garante que só
            // a primeira chamada neste processo chega aqui, então ninguém mais
            // é dono dele (nem uma chamada anterior, que já o fechou). O
            // `OwnedFd` passa a ser o único dono e o fecha depois de
            // `listener_from_fd` reabri-lo.
            #[allow(unsafe_code)]
            let fd = unsafe { OwnedFd::from_raw_fd(LISTEN_FDS_START) };
            let listener = listener_from_fd(fd).map_err(|e| {
                ServiceError::Activation(format!("O socket passado pelo systemd não serve: {e}"))
            })?;
            let listener = tokio::net::UnixListener::from_std(listener)?;
            Ok((listener, Activation::Systemd))
        }
        n => Err(ServiceError::Activation(format!(
            "O systemd passou {n} sockets; o serviço espera um só."
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lucerna_paths::Env;

    fn paths(dir: &Path) -> AppPaths {
        let env = Env {
            xdg_runtime_dir: Some(dir.to_path_buf()),
            ..Env::default()
        };
        AppPaths::new("hello", &env).unwrap()
    }

    #[tokio::test]
    async fn systemd_fd_is_taken_only_once() {
        // Simula a primeira chamada sem tocar no fd 3 do processo de teste.
        TAKEN.store(true, Ordering::SeqCst);
        let t = tempfile::tempdir().unwrap();
        let pid = std::process::id().to_string();
        let err = listener_from_vars(&paths(t.path()), Some(&pid), Some("1")).unwrap_err();
        assert!(matches!(&err, ServiceError::Activation(_)));
        assert_eq!(
            err.to_string(),
            "O socket do systemd já foi usado neste processo."
        );
    }

    #[tokio::test]
    async fn more_than_one_fd_is_an_error() {
        let t = tempfile::tempdir().unwrap();
        let pid = std::process::id().to_string();
        let err = listener_from_vars(&paths(t.path()), Some(&pid), Some("2")).unwrap_err();
        assert!(matches!(err, ServiceError::Activation(_)));
    }

    #[tokio::test]
    async fn zero_fds_binds_manually() {
        let t = tempfile::tempdir().unwrap();
        let pid = std::process::id().to_string();
        let (_l, a) = listener_from_vars(&paths(t.path()), Some(&pid), Some("0")).unwrap();
        assert!(matches!(a, Activation::Manual(_)));
        assert!(t.path().join("lucerna").join("hello.sock").exists());
    }
}
