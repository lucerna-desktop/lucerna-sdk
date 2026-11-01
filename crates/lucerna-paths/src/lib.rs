#![warn(missing_docs)]
//! Caminhos XDG e do socket de cada app do Lucerna.
//!
//! Regras puras: nada aqui lê o ambiente sozinho além de [`Env::from_process`];
//! os testes passam um [`Env`] explícito.
//!
//! Além das pastas XDG de dados, configuração e cache, há a pasta de estado
//! (`$XDG_STATE_HOME/lucerna/<app>`), onde fica o log do serviço subido pelo
//! cliente, e a entrada de início automático em `$XDG_CONFIG_HOME/autostart`.
//!
//! **Protocolo:** não fala o protocolo; as pastas e o socket
//! (`$XDG_RUNTIME_DIR/lucerna/<app>.sock`) são os do protocolo 1.0.

use std::{
    fs, io,
    os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

/// Pasta de manifestos do sistema.
const SYSTEM_MANIFEST_DIR: &str = "/usr/share/lucerna/apps";

/// As variáveis de ambiente que definem as pastas.
#[derive(Default, Clone, Debug)]
pub struct Env {
    /// `HOME`.
    pub home: Option<PathBuf>,
    /// `XDG_DATA_HOME`.
    pub xdg_data_home: Option<PathBuf>,
    /// `XDG_CONFIG_HOME`.
    pub xdg_config_home: Option<PathBuf>,
    /// `XDG_CACHE_HOME`.
    pub xdg_cache_home: Option<PathBuf>,
    /// `XDG_STATE_HOME`.
    pub xdg_state_home: Option<PathBuf>,
    /// `XDG_RUNTIME_DIR`.
    pub xdg_runtime_dir: Option<PathBuf>,
}

impl Env {
    /// Lê as variáveis do processo; variáveis vazias contam como ausentes.
    pub fn from_process() -> Env {
        Env::from_lookup(|name| std::env::var_os(name))
    }

    /// Monta o `Env` a partir de uma função de consulta; valores vazios
    /// contam como ausentes.
    pub fn from_lookup(get: impl Fn(&str) -> Option<std::ffi::OsString>) -> Env {
        let var = |name: &str| get(name).filter(|v| !v.is_empty()).map(PathBuf::from);
        Env {
            home: var("HOME"),
            xdg_data_home: var("XDG_DATA_HOME"),
            xdg_config_home: var("XDG_CONFIG_HOME"),
            xdg_cache_home: var("XDG_CACHE_HOME"),
            xdg_state_home: var("XDG_STATE_HOME"),
            xdg_runtime_dir: var("XDG_RUNTIME_DIR"),
        }
    }

    /// Os pares `(nome, valor)` das variáveis definidas, na ordem `HOME`,
    /// `XDG_DATA_HOME`, `XDG_CONFIG_HOME`, `XDG_CACHE_HOME`, `XDG_STATE_HOME`,
    /// `XDG_RUNTIME_DIR`; as ausentes ficam de fora e os valores vão como estão.
    pub fn vars(&self) -> Vec<(&'static str, PathBuf)> {
        [
            ("HOME", &self.home),
            ("XDG_DATA_HOME", &self.xdg_data_home),
            ("XDG_CONFIG_HOME", &self.xdg_config_home),
            ("XDG_CACHE_HOME", &self.xdg_cache_home),
            ("XDG_STATE_HOME", &self.xdg_state_home),
            ("XDG_RUNTIME_DIR", &self.xdg_runtime_dir),
        ]
        .into_iter()
        .filter_map(|(name, v)| v.clone().map(|v| (name, v)))
        .collect()
    }

    /// Variável XDG só vale se for caminho absoluto.
    fn absolute(p: &Option<PathBuf>) -> Option<&Path> {
        p.as_deref().filter(|p| p.is_absolute())
    }

    fn base(&self, xdg: &Option<PathBuf>, fallback: &str) -> Result<PathBuf, PathsError> {
        if let Some(p) = Self::absolute(xdg) {
            return Ok(p.to_path_buf());
        }
        let home = Self::absolute(&self.home).ok_or(PathsError::NoHome)?;
        Ok(home.join(fallback))
    }

    fn data_home(&self) -> Result<PathBuf, PathsError> {
        self.base(&self.xdg_data_home, ".local/share")
    }

    /// `$XDG_CONFIG_HOME` (se absoluto) ou `$HOME/.config`.
    pub fn config_home(&self) -> Result<PathBuf, PathsError> {
        self.base(&self.xdg_config_home, ".config")
    }

    fn cache_home(&self) -> Result<PathBuf, PathsError> {
        self.base(&self.xdg_cache_home, ".cache")
    }

    /// `$XDG_STATE_HOME` (se absoluto) ou `$HOME/.local/state`.
    pub fn state_home(&self) -> Result<PathBuf, PathsError> {
        self.base(&self.xdg_state_home, ".local/state")
    }

    /// `$XDG_RUNTIME_DIR`, se absoluto; senão [`PathsError::NoRuntimeDir`]
    /// (nunca `/tmp`).
    pub fn runtime_dir(&self) -> Result<PathBuf, PathsError> {
        Self::absolute(&self.xdg_runtime_dir)
            .map(Path::to_path_buf)
            .ok_or(PathsError::NoRuntimeDir)
    }

    fn runtime_lucerna(&self) -> Result<PathBuf, PathsError> {
        Ok(self.runtime_dir()?.join("lucerna"))
    }
}

/// Erros ao resolver caminhos.
#[derive(Debug, thiserror::Error)]
pub enum PathsError {
    /// O id do app não segue `^[a-z][a-z0-9-]{0,31}$`.
    #[error("Nome de app inválido: \"{0}\" (use a-z, 0-9 e \"-\", começando por letra, até 32).")]
    InvalidAppId(String),
    /// Sem `HOME` (absoluto) e sem a variável XDG correspondente.
    #[error("HOME não está definido.")]
    NoHome,
    /// `XDG_RUNTIME_DIR` ausente ou relativo.
    #[error("XDG_RUNTIME_DIR não está definido.")]
    NoRuntimeDir,
    /// O caminho do socket não cabe em `sun_path` (108 bytes com o `\0` final).
    #[error(
        "O caminho do socket é longo demais ({len} bytes; o limite é {max}): {}.",
        path.display(),
        max = SOCKET_PATH_MAX
    )]
    SocketPathTooLong {
        /// O caminho que não coube.
        path: PathBuf,
        /// O tamanho dele em bytes.
        len: usize,
    },
}

/// Maior caminho de socket Unix no Linux, em bytes: `sun_path` tem 108, com o
/// `\0` final.
pub const SOCKET_PATH_MAX: usize = 107;

/// Confere que o caminho cabe num endereço de socket Unix.
fn fits_sun_path(path: PathBuf) -> Result<PathBuf, PathsError> {
    let len = path.as_os_str().len();
    if len > SOCKET_PATH_MAX {
        Err(PathsError::SocketPathTooLong { path, len })
    } else {
        Ok(path)
    }
}

/// Confere o id do app: `^[a-z][a-z0-9-]{0,31}$`. O id `shell` é reservado
/// (o socket colidiria com o `shell.sock`).
pub fn validate_app_id(id: &str) -> Result<(), PathsError> {
    let bytes = id.as_bytes();
    let ok = id != "shell"
        && !bytes.is_empty()
        && bytes.len() <= 32
        && bytes[0].is_ascii_lowercase()
        && bytes[1..]
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-');
    if ok {
        Ok(())
    } else {
        Err(PathsError::InvalidAppId(id.to_string()))
    }
}

/// As pastas e o socket de um app.
#[derive(Clone, Debug)]
pub struct AppPaths {
    app: String,
    env: Env,
}

impl AppPaths {
    /// Cria os caminhos do app `app`, validando o id.
    pub fn new(app: &str, env: &Env) -> Result<AppPaths, PathsError> {
        validate_app_id(app)?;
        Ok(AppPaths {
            app: app.to_string(),
            env: env.clone(),
        })
    }

    /// O id do app.
    pub fn app(&self) -> &str {
        &self.app
    }

    /// `$XDG_DATA_HOME/lucerna/<app>`.
    pub fn data_dir(&self) -> Result<PathBuf, PathsError> {
        Ok(self.env.data_home()?.join("lucerna").join(&self.app))
    }

    /// `$XDG_CONFIG_HOME/lucerna/<app>.toml`.
    pub fn config_file(&self) -> Result<PathBuf, PathsError> {
        Ok(self
            .env
            .config_home()?
            .join("lucerna")
            .join(format!("{}.toml", self.app)))
    }

    /// `$XDG_CACHE_HOME/lucerna/<app>`.
    pub fn cache_dir(&self) -> Result<PathBuf, PathsError> {
        Ok(self.env.cache_home()?.join("lucerna").join(&self.app))
    }

    /// `$XDG_STATE_HOME/lucerna/<app>`.
    pub fn state_dir(&self) -> Result<PathBuf, PathsError> {
        Ok(self.env.state_home()?.join("lucerna").join(&self.app))
    }

    /// `$XDG_STATE_HOME/lucerna/<app>/serve.log`: o log do serviço subido
    /// pelo cliente.
    pub fn serve_log(&self) -> Result<PathBuf, PathsError> {
        Ok(self.state_dir()?.join("serve.log"))
    }

    /// `$XDG_CONFIG_HOME/autostart/lucerna-<app>.desktop`.
    pub fn autostart_file(&self) -> Result<PathBuf, PathsError> {
        Ok(self
            .env
            .config_home()?
            .join("autostart")
            .join(format!("lucerna-{}.desktop", self.app)))
    }

    /// `$XDG_RUNTIME_DIR/lucerna`.
    pub fn runtime_dir(&self) -> Result<PathBuf, PathsError> {
        self.env.runtime_lucerna()
    }

    /// `$XDG_RUNTIME_DIR/lucerna/<app>.sock`; [`PathsError::SocketPathTooLong`]
    /// se passar de [`SOCKET_PATH_MAX`] bytes.
    pub fn socket(&self) -> Result<PathBuf, PathsError> {
        fits_sun_path(
            self.env
                .runtime_lucerna()?
                .join(format!("{}.sock", self.app)),
        )
    }
}

/// `$XDG_RUNTIME_DIR/lucerna/shell.sock`; [`PathsError::SocketPathTooLong`]
/// se passar de [`SOCKET_PATH_MAX`] bytes.
pub fn shell_socket(env: &Env) -> Result<PathBuf, PathsError> {
    fits_sun_path(env.runtime_lucerna()?.join("shell.sock"))
}

/// Pastas de manifestos: a do usuário primeiro, depois a do sistema.
pub fn manifest_dirs(env: &Env) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(data) = env.data_home() {
        dirs.push(data.join("lucerna").join("apps"));
    }
    dirs.push(PathBuf::from(SYSTEM_MANIFEST_DIR));
    dirs
}

/// Cria a pasta com modo `0700`, corrigindo o modo se ela já existia.
///
/// Só o último nível é privado: os pais são criados com o modo padrão. Se o
/// caminho já existir, precisa ser uma pasta de verdade (nem link simbólico,
/// nem arquivo) e pertencer ao usuário atual; senão devolve erro.
pub fn create_private_dir(dir: &Path) -> io::Result<()> {
    if let Some(parent) = dir.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    match fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => return Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let md = fs::symlink_metadata(dir)?;
    if !md.file_type().is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} existe e não é uma pasta de verdade", dir.display()),
        ));
    }
    if md.uid() != fs::metadata("/proc/self")?.uid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} pertence a outro usuário", dir.display()),
        ));
    }
    if md.permissions().mode() & 0o777 != 0o700 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
