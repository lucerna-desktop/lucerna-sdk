# lucerna-hello

O app de exemplo do Lucerna: um contador. É o molde que os apps copiam.

## Como é feito

- **lib + bin.** `src/lib.rs` monta tudo e é o que os testes usam: `hello(env: &Env)`
  devolve o `ServiceBuilder` com os tópicos e os métodos, e `cli()` devolve a
  linha de comando (`lucerna_cli::CliApp`). `src/main.rs` só escolhe entre
  `lucerna-hello serve` (o serviço) e os subcomandos. O `serve` que perde a
  corrida para outro serviço (`ServiceError::AlreadyRunning`) sai com `0`, em
  silêncio.
- **Tópicos.** `counter` (`{"value": n}`) e o contínuo `ticks`
  (`{"tick": k, "bands": [64 × f32]}` a cada 1 ms, produzido só com assinante).
- **Métodos.** `counter.add {"n"}`, `counter.reset`, `quiet.set {"on"}`,
  `reminder.set {"in", "persistent"?}` e `reminder.cancel`.
- **Lembrete.** O tópico `reminder` (`{"at", "persistent", "last_fired"}`)
  mostra o próximo horário; o estado fica em `reminder.json`: o persistente
  na pasta de dados (sobrevive a reiniciar), o outro em
  `$XDG_RUNTIME_DIR/lucerna/hello` (some no logout e ao reiniciar, como o
  timer não persistente do systemd). Há um lembrete por vez. O horário é
  marcado no timer `reminder` (a unidade `lucerna-hello_reminder.timer`, com
  o systemd) ou, sem ele, no agendador local, que mantém o serviço de pé até
  lá. No horário, o serviço tira o lembrete e publica o disparo no tópico
  `reminder` (`last_fired`); um app de verdade avisaria também pelas
  notificações do sistema. Um lembrete vencido com o serviço parado é
  cumprido quando ele sobe de novo. O `--persistent` mantém também a entrada
  de autostart até o disparo.
- **Shell.** Com o contador diferente de zero, o item `counter` fica na barra
  com o valor; `quiet.set` pede ou devolve o não perturbe. Os dois seguram o
  serviço (ele não sai por ociosidade enquanto existirem). Sem shell, nada dá
  erro: o estado é reaplicado quando ele volta.
- **Testes.** `tests/protocol.rs` fala o protocolo com o serviço em processo,
  num socket temporário; `tests/shell.rs` usa o `FakeShell` do
  `lucerna-service` (feature `testing`, ligada em `[dev-dependencies]`);
  `tests/reminder.rs` cobre o lembrete (disparo no horário, persistente, onde
  fica o arquivo, vencido ao subir, pedidos inválidos, `remind` e `forget`); `tests/files.rs` confere o
  manifesto e as unidades; `tests/process.rs` roda o binário de verdade, sem
  systemd (o `PATH` não tem `systemctl`, ou tem um falso): a CLI sobe o
  serviço, dois clientes juntos acabam num serviço só, o próximo cliente o
  sobe de novo depois de uma queda e o lembrete acontece pelo agendador local.

## Linha de comando

```sh
lucerna-hello add 3        # Contador: 3
lucerna-hello reset        # Contador: 0
lucerna-hello quiet on     # liga o não perturbe
lucerna-hello remind 30    # lembrete para daqui a 30 s (--persistent sobrevive a reiniciar)
lucerna-hello forget       # tira o lembrete
lucerna-hello status       # estado dos tópicos
lucerna-hello watch counter
```

## Instalação

O binário vai para `/usr/bin/lucerna-hello`; as unidades e o manifesto, para
as pastas do systemd do usuário e do Lucerna:

```sh
sudo install -Dm755 target/release/lucerna-hello /usr/bin/lucerna-hello
sudo install -Dm644 systemd/lucerna-hello.socket systemd/lucerna-hello.service \
    -t /usr/lib/systemd/user/
sudo install -Dm644 data/hello.json /usr/share/lucerna/apps/hello.json
systemctl --user daemon-reload
systemctl --user enable --now lucerna-hello.socket
```

Num pacote, troque `/usr` pelo prefixo do empacotador (ex.: `$pkgdir/usr`) e
tire o `sudo`; o `ExecStart=` da unidade aponta para `/usr/bin`.

O systemd escuta em `$XDG_RUNTIME_DIR/lucerna/hello.sock` (pasta `0700`,
socket `0600`) e sobe o serviço na primeira conexão; o serviço sai sozinho
depois de 60 s sem clientes, sem itens na barra e sem não perturbe pedido.

## Sem instalar nada

Sem o systemd e sem instalar unidades, `lucerna-hello add 3` já sobe o
serviço sozinho (o log fica em `$XDG_STATE_HOME/lucerna/hello/serve.log`), e o
próximo comando o sobe de novo se ele cair. Nada sobe no login, a não ser
com um lembrete `--persistent` marcado: enquanto ele existe, a entrada
`$XDG_CONFIG_HOME/autostart/lucerna-hello.desktop` sobe o serviço no login do
KDE, do GNOME e do XFCE para cumprir o lembrete vencido, e some depois do
disparo. No Hyprland sem systemd, que não lê essa pasta, o mesmo papel fica
com uma linha na configuração (sem nada a fazer, o serviço sai por
ociosidade):

```
exec-once = lucerna-hello serve
```

`LUCERNA_NO_SPAWN=1` desliga a subida pela linha de comando.

Para desenvolver sem instalar, `cargo run -p lucerna-hello -- serve` cria o
socket à mão no mesmo lugar (`LUCERNA_IDLE_TIMEOUT_MS` encurta a ociosidade).
O manifesto do usuário, em `$XDG_DATA_HOME/lucerna/apps/hello.json`, vence o
do sistema.
