//! Cliente do protocolo: aperto de mão, chamadas concorrentes e assinaturas.
//!
//! Uma tarefa leitora por conexão despacha as respostas pelo `id` e guarda os
//! `lucerna.state` num mapa pendente com no máximo um estado por tópico (o mais
//! novo), como o servidor faz: a leitora nunca espera, então não ler
//! [`Client::next_state`] não trava as chamadas. Por isso estados intermediários
//! de um tópico podem ser pulados; cada estado é o retrato inteiro do tópico.
//!
//! O `seq` volta a `0` quando o serviço reinicia: o último `seq` visto é por
//! conexão, e um cliente que reconecta (um `Client` novo) começa do zero.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lucerna_protocol::methods::{HELLO, STATE, SUBSCRIBE, UNSUBSCRIBE};
use lucerna_protocol::{
    HelloParams, HelloResult, Id, Message, Notification, PROTOCOL_VERSION, Request, RpcError,
    State, SubscribeResult, TopicsParams, codes, to_value,
};
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{Notify, oneshot, watch};
use tokio::task::JoinHandle;

use crate::framing::{FrameError, LineReader, MAX_MESSAGE};
use crate::spawn::{SPAWN_WAIT, ServeCommand};
use crate::sync::lock;

/// Quanto uma escrita que falhou com o socket quebrado espera pelo erro que o
/// serviço tenha mandado antes de fechar.
const FATAL_GRACE: Duration = Duration::from_secs(1);

/// Erro do cliente. As mensagens são frases em português, com maiúscula e
/// ponto final, prontas para o usuário.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// Não deu para abrir o socket (o serviço não está lá).
    #[error("Não foi possível conectar ao serviço: {0}.")]
    Connect(io::Error),
    /// Erro de E/S com a conexão aberta.
    ///
    /// Inclui o `EPIPE` de uma escrita quando o serviço fechou sem mandar erro.
    #[error("Erro de comunicação com o serviço: {0}.")]
    Io(io::Error),
    /// O serviço respondeu com erro. Um `-32010` no aperto de mão também quer
    /// dizer que não dá para falar com o serviço.
    #[error("{0}")]
    Rpc(RpcError),
    /// A conexão fechou antes da resposta.
    #[error("O serviço fechou a conexão.")]
    Closed,
    /// O prazo de `connect_timeout` ou `call_timeout` passou sem resposta.
    #[error("O serviço não respondeu a tempo.")]
    Timeout,
    /// O serviço respondeu, mas o resultado não tem a forma esperada
    /// (ex.: um `hello` sem os campos do protocolo). O serviço existe e falou:
    /// não é "sem serviço".
    #[error("Resposta inválida do serviço: {0}.")]
    Decode(serde_json::Error),
    /// [`Client::connect_or_spawn`] subiu o serviço, mas ele não atendeu no
    /// socket em [`SPAWN_WAIT`]. O porquê costuma estar no `log`.
    #[error("O serviço não subiu em 2 s; veja o log em {}.", log.display())]
    NotStarted {
        /// O log do serviço subido.
        log: PathBuf,
    },
    /// [`Client::connect_or_spawn`] não conseguiu rodar `program serve`.
    #[error("Não foi possível rodar {} serve: {source}.", program.display())]
    Spawn {
        /// O programa que não rodou.
        program: PathBuf,
        /// O erro do sistema.
        source: io::Error,
    },
}

/// Espera antes da tentativa `n` (a partir de `0`): `10 ms × 2^n`, no máximo `200 ms`.
fn backoff(n: u32) -> Duration {
    Duration::from_millis(10u64.saturating_mul(1u64 << n.min(16)).min(200))
}

/// O `connect` falhou porque ninguém ouve no socket (arquivo ausente ou socket velho)?
fn nobody_listening(e: &ClientError) -> bool {
    matches!(
        e,
        ClientError::Connect(io) if matches!(
            io.kind(),
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
        )
    )
}

/// Quem espera uma resposta.
struct Waiter {
    tx: oneshot::Sender<Result<Value, RpcError>>,
    /// Resposta de um `lucerna.subscribe`: o retrato atualiza o último `seq` visto.
    subscribe: bool,
}

/// Estado compartilhado entre o `Client` e a tarefa leitora.
struct Calls {
    /// `false` depois que a leitora terminou: ninguém mais vai responder.
    open: bool,
    waiting: HashMap<i64, Waiter>,
    /// `-32000` (sem `id`) que o serviço mandou antes de fechar.
    fatal: Option<RpcError>,
    /// Estados pendentes: no máximo um por tópico (o mais novo).
    states: HashMap<String, State>,
    /// Tópicos de `states` na ordem em que chegaram.
    order: VecDeque<String>,
}

type Shared = Arc<Mutex<Calls>>;

/// Tira o pedido do mapa se a chamada for cancelada antes da resposta.
struct Forget<'a> {
    calls: &'a Shared,
    id: i64,
}

impl Drop for Forget<'_> {
    fn drop(&mut self) {
        lock(self.calls).waiting.remove(&self.id);
    }
}

/// Cliente de um serviço do Lucerna, por conexão.
pub struct Client {
    hello: HelloResult,
    writer: Arc<tokio::sync::Mutex<OwnedWriteHalf>>,
    next_id: AtomicI64,
    calls: Shared,
    /// Acordado a cada estado novo e quando a conexão fecha.
    ready: Arc<Notify>,
    closed: watch::Receiver<bool>,
    reader: JoinHandle<()>,
}

impl Drop for Client {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

impl Client {
    /// Conecta ao socket em `path` e faz o aperto de mão como `client` com o app `app`.
    pub async fn connect(path: &Path, client: &str, app: &str) -> Result<Client, ClientError> {
        let stream = UnixStream::connect(path)
            .await
            .map_err(ClientError::Connect)?;
        let (rd, wr) = stream.into_split();
        let calls: Shared = Arc::new(Mutex::new(Calls {
            open: true,
            waiting: HashMap::new(),
            fatal: None,
            states: HashMap::new(),
            order: VecDeque::new(),
        }));
        let ready = Arc::new(Notify::new());
        let (closed_tx, closed) = watch::channel(false);
        let reader = tokio::spawn(read_loop(
            LineReader::new(rd, MAX_MESSAGE),
            calls.clone(),
            ready.clone(),
            closed_tx,
        ));
        let mut c = Client {
            hello: HelloResult {
                protocol: PROTOCOL_VERSION,
                app: String::new(),
                version: String::new(),
                topics: Vec::new(),
                streams: Vec::new(),
                methods: Vec::new(),
            },
            writer: Arc::new(tokio::sync::Mutex::new(wr)),
            next_id: AtomicI64::new(1),
            calls,
            ready,
            closed,
            reader,
        };
        let params = HelloParams {
            protocol: PROTOCOL_VERSION,
            client: client.to_string(),
            app: app.to_string(),
        };
        let result = c.request(HELLO, Some(to_value(&params)), false).await?;
        c.hello = from_value(result)?;
        Ok(c)
    }

    /// Como [`Client::connect`], com prazo `dur` para abrir o socket e fazer o
    /// aperto de mão juntos. Passou do prazo: [`ClientError::Timeout`].
    pub async fn connect_timeout(
        path: &Path,
        client: &str,
        app: &str,
        dur: Duration,
    ) -> Result<Client, ClientError> {
        tokio::time::timeout(dur, Client::connect(path, client, app))
            .await
            .map_err(|_| ClientError::Timeout)?
    }

    /// Como [`Client::connect`], mas sobe o serviço com `serve` quando ninguém
    /// atende no socket (o arquivo não existe ou é um socket velho).
    ///
    /// Depois de subir, tenta de novo com espera crescente (`10 ms × 2^n`, no
    /// máximo `200 ms`) até [`SPAWN_WAIT`]; passou o prazo:
    /// [`ClientError::NotStarted`] com o log. Não rodou o programa:
    /// [`ClientError::Spawn`]. Os outros erros (permissão, um aperto de mão
    /// recusado) voltam como estão, sem subir nada. Não há prazo para o aperto
    /// de mão: quem chama põe o seu, como a CLI faz.
    pub async fn connect_or_spawn(
        path: &Path,
        client: &str,
        app: &str,
        serve: &ServeCommand,
    ) -> Result<Client, ClientError> {
        match Client::connect(path, client, app).await {
            Err(e) if nobody_listening(&e) => {}
            other => return other,
        }
        serve.spawn().map_err(|source| ClientError::Spawn {
            program: serve.program.clone(),
            source,
        })?;
        let start = Instant::now();
        let mut n = 0;
        loop {
            let left = SPAWN_WAIT.saturating_sub(start.elapsed());
            if left.is_zero() {
                return Err(ClientError::NotStarted {
                    log: serve.log.clone(),
                });
            }
            tokio::time::sleep(backoff(n).min(left)).await;
            n += 1;
            match Client::connect(path, client, app).await {
                Err(e) if nobody_listening(&e) => {}
                other => return other,
            }
        }
    }

    /// Resposta do aperto de mão.
    pub fn hello(&self) -> &HelloResult {
        &self.hello
    }

    /// Chama `method` e espera a resposta. Pode ser chamado de várias tarefas ao mesmo tempo.
    pub async fn call(&self, method: &str, params: Option<Value>) -> Result<Value, ClientError> {
        self.request(method, params, false).await
    }

    /// Como [`Client::call`], com prazo `dur` para a resposta. Passou do prazo:
    /// [`ClientError::Timeout`] (a conexão continua; uma resposta atrasada é descartada).
    pub async fn call_timeout(
        &self,
        method: &str,
        params: Option<Value>,
        dur: Duration,
    ) -> Result<Value, ClientError> {
        tokio::time::timeout(dur, self.call(method, params))
            .await
            .map_err(|_| ClientError::Timeout)?
    }

    /// Manda uma notificação (sem resposta).
    pub async fn notify(&self, method: &str, params: Option<Value>) -> Result<(), ClientError> {
        if !lock(&self.calls).open {
            return Err(ClientError::Closed);
        }
        let msg = Message::Notification(Notification {
            method: method.to_string(),
            params,
        });
        self.send(&msg).await.map_err(ClientError::Io)
    }

    /// Assina `topics` e devolve o retrato atual de cada um.
    ///
    /// As mudanças seguintes chegam por [`Client::next_state`].
    pub async fn subscribe(&self, topics: &[&str]) -> Result<Vec<State>, ClientError> {
        let result = self
            .request(SUBSCRIBE, Some(topics_params(topics)), true)
            .await?;
        let r: SubscribeResult = from_value(result)?;
        Ok(r.states)
    }

    /// Cancela as assinaturas de `topics` e descarta os estados pendentes deles.
    pub async fn unsubscribe(&self, topics: &[&str]) -> Result<(), ClientError> {
        self.request(UNSUBSCRIBE, Some(topics_params(topics)), false)
            .await?;
        // Depois da resposta: o servidor não manda mais estados desses tópicos.
        let mut calls = lock(&self.calls);
        for t in topics {
            calls.states.remove(*t);
        }
        let Calls { states, order, .. } = &mut *calls;
        order.retain(|t| states.contains_key(t));
        Ok(())
    }

    /// Próximo estado de um tópico assinado: o tópico que espera há mais tempo,
    /// com o retrato mais novo dele. Estados intermediários podem ter sido pulados
    /// (coalescidos); os que chegam com `seq` já visto são descartados.
    /// `None` quando a conexão fechou e não há mais estados pendentes.
    ///
    /// Recebe `&self`: uma tarefa pode ler os estados enquanto outras chamam
    /// métodos no mesmo `Arc<Client>`. Com várias tarefas lendo ao mesmo tempo,
    /// cada estado vai para uma só delas.
    pub async fn next_state(&self) -> Option<State> {
        loop {
            // Registrado antes de olhar o mapa: um estado (ou o fechamento) que
            // chegue entre a conferência e a espera não se perde.
            let notified = self.ready.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut calls = lock(&self.calls);
                if let Some(topic) = calls.order.pop_front() {
                    if let Some(st) = calls.states.remove(&topic) {
                        return Some(st);
                    }
                    continue;
                }
                if !calls.open {
                    return None;
                }
            }
            notified.await;
        }
    }

    /// Volta quando a conexão fecha.
    pub async fn closed(&self) {
        let mut rx = self.closed.clone();
        // Erro aqui é a leitora ter terminado: a conexão também fechou.
        let _ = rx.wait_for(|closed| *closed).await;
    }

    async fn request(
        &self,
        method: &str,
        params: Option<Value>,
        subscribe: bool,
    ) -> Result<Value, ClientError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut calls = lock(&self.calls);
            if !calls.open {
                return Err(ClientError::Closed);
            }
            calls.waiting.insert(id, Waiter { tx, subscribe });
        }
        let _forget = Forget {
            calls: &self.calls,
            id,
        };
        let msg = Message::Request(Request {
            id: Id::Num(id),
            method: method.to_string(),
            params,
        });
        if let Err(e) = self.send(&msg).await {
            // Uma mensagem grande demais pode dar `EPIPE` antes de lermos o
            // `-32000` que o serviço mandou: espera a leitora chegar ao fim.
            if is_broken(&e) {
                let _ = tokio::time::timeout(FATAL_GRACE, self.closed()).await;
                if let Some(f) = lock(&self.calls).fatal.clone() {
                    return Err(ClientError::Rpc(f));
                }
            }
            return Err(ClientError::Io(e));
        }
        match rx.await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(ClientError::Rpc(e)),
            Err(_) => Err(lock(&self.calls)
                .fatal
                .clone()
                .map_or(ClientError::Closed, ClientError::Rpc)),
        }
    }

    /// Escreve uma linha numa tarefa própria: cancelar quem chamou não deixa
    /// uma linha pela metade no fluxo.
    async fn send(&self, msg: &Message) -> io::Result<()> {
        let mut bytes = msg.to_json().into_bytes();
        bytes.push(b'\n');
        let writer = self.writer.clone();
        tokio::spawn(async move {
            let mut w = writer.lock().await;
            w.write_all(&bytes).await?;
            w.flush().await
        })
        .await
        .map_err(io::Error::other)?
    }
}

fn is_broken(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
    )
}

fn topics_params(topics: &[&str]) -> Value {
    to_value(&TopicsParams {
        topics: topics.iter().map(|t| t.to_string()).collect(),
    })
}

fn from_value<T: serde::de::DeserializeOwned>(v: Value) -> Result<T, ClientError> {
    serde_json::from_value(v).map_err(ClientError::Decode)
}

/// Lê as mensagens do serviço até a conexão fechar.
async fn read_loop(
    mut lines: LineReader<OwnedReadHalf>,
    calls: Shared,
    ready: Arc<Notify>,
    closed: watch::Sender<bool>,
) {
    let mut seen: HashMap<String, u64> = HashMap::new();
    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Err(FrameError::InvalidUtf8) => continue,
            // Fim do fluxo, `ECONNRESET` (o serviço fechou com dados não lidos),
            // outro erro de E/S ou linha grande demais: a conexão acabou.
            Ok(None) | Err(_) => break,
        };
        match Message::parse(&line) {
            Ok(Message::Response(r)) => match r.id {
                Some(Id::Num(id)) => {
                    let Some(w) = lock(&calls).waiting.remove(&id) else {
                        continue;
                    };
                    if w.subscribe
                        && let Ok(v) = &r.outcome
                        && let Ok(s) = serde_json::from_value::<SubscribeResult>(v.clone())
                    {
                        let mut c = lock(&calls);
                        for st in s.states {
                            // Ao reassinar, um pendente até o `seq` do retrato
                            // sairia depois dele: fica para trás.
                            if c.states.get(&st.topic).is_some_and(|p| p.seq <= st.seq) {
                                c.states.remove(&st.topic);
                            }
                            let last = seen.entry(st.topic).or_insert(st.seq);
                            *last = (*last).max(st.seq);
                        }
                        let Calls { states, order, .. } = &mut *c;
                        order.retain(|t| states.contains_key(t));
                    }
                    let _ = w.tx.send(r.outcome);
                }
                None => {
                    if let Err(e) = r.outcome
                        && e.code == codes::MESSAGE_TOO_LARGE
                    {
                        lock(&calls).fatal = Some(e);
                    }
                }
                Some(Id::Str(_)) => {}
            },
            Ok(Message::Notification(n)) if n.method == STATE => {
                let Some(st) = n
                    .params
                    .and_then(|p| serde_json::from_value::<State>(p).ok())
                else {
                    continue;
                };
                if seen.get(&st.topic).is_some_and(|&last| st.seq <= last) {
                    continue;
                }
                seen.insert(st.topic.clone(), st.seq);
                {
                    let mut c = lock(&calls);
                    if !c.states.contains_key(&st.topic) {
                        c.order.push_back(st.topic.clone());
                    }
                    c.states.insert(st.topic.clone(), st);
                }
                ready.notify_one();
            }
            _ => {}
        }
    }
    {
        let mut c = lock(&calls);
        c.open = false;
        // Largar os `oneshot` acorda quem espera com "conexão fechou".
        c.waiting.clear();
    }
    // Todos os que esperam em `next_state` precisam ver o fim.
    ready.notify_waiters();
    closed.send_replace(true);
}
