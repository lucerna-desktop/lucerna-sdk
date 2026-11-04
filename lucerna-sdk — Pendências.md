# lucerna-sdk — Pendências

O que falta fazer no `lucerna-sdk`. Cada item vira um PR; ao terminar, ele é marcado aqui.

Os itens vêm do "Trabalho seguinte" do [plano do serviço sem systemd](docs/superpowers/plans/2026-11-01-servico-sem-systemd.md) e do PR #14, das Pendências do [`lucerna-shell`](https://github.com/lucerna-desktop/lucerna-shell) (até 2026-11-04 elas guardavam também os itens dos outros repositórios) e do [spec do `lucerna-files`](https://github.com/lucerna-desktop/lucerna-files/blob/main/docs/superpowers/specs/2026-10-27-explorador-design.md). Cada repositório do ecossistema tem o seu arquivo de pendências na raiz.

## Serviço sem systemd

Do PR #14 (2026-11-01).

- [ ] **Com o systemd escolhido, o timer que dispara com o serviço já de pé não avisa o app:** o `on_timer` só é chamado pelo agendador local, e o `on_start` só roda na subida. O trabalho vencido espera a próxima subida. Já existia antes do PR #14.
- [ ] **Cliente que chega na saída por ociosidade do serviço anterior:** com o socket já fechado e o lock ainda preso, o `serve` novo é recusado com `AlreadyRunning`, e o cliente pode desistir com o código `2` e o log vazio; uma conexão aceita da fila e logo fechada volta `Closed`, sem subida nenhuma. Correção possível: subir de novo quando o primeiro filho já saiu, ou tratar o `Closed` no aperto de mão como "ninguém ouvindo".
- [ ] **A TUI de cada app, quando existir, usa o `Client::connect_or_spawn`** e mostra a causa do `NotStarted` em vez de sair. O trabalho é no repositório de cada app; fica aqui como regra até o molde de TUI existir no SDK.

Nos outros repositórios:

- O **host de painéis do `lucerna-shell`** sobe o serviço quando a conexão ao `<app>.sock` é recusada (item 7 do PR #13): está nas Pendências do `lucerna-shell`.
- O **`lucernad` adota o `connect_or_spawn` e o agendador** quando migrar para o SDK: está nas Pendências do `lucernad`.

## Linha de comando

- [ ] **`status` sem a primeira amostra:** o `status` do `lucerna-cli` (no `lucernad`, o `lucernad status`) imprime o retrato inicial dos tópicos amostrados (`data: null`, `seq` 0) e sai, então `system.stats`, `system.disks` e `system.gpus` aparecem vazios. Ele deveria esperar a primeira amostra por um prazo curto. Veio das sobras do módulo de sistema do `lucernad`.
- [ ] **Comandos de linha para todo o ecossistema, em inglês** (pedido do usuário, 2026-11-01). Ter comandos para as principais funções do shell inteiro e dos apps (notes, routine, phone, files e os próximos), com nomes padronizados em inglês (subcomandos, opções e métodos). Levantar o que existe hoje (o `qs ipc` do shell, `lucernad` com subcomandos em português como `restaurar-jogo`, os `lucerna-<app>` pelo `lucerna-cli`), desenhar um padrão comum (verbos, nomes, saída em texto e `--json`, códigos de saída) no [`docs/ecossistema.md`](docs/ecossistema.md) e migrar os nomes em português mantendo apelidos por um tempo. Passa por brainstorming e spec. A migração acontece em cada repositório.

## Para o explorador (`lucerna-files`)

Da divisão por fases do spec do explorador (2026-10-27).

- [ ] **v2:** `lucerna-gui`, `Lucerna.Ui` e `lucerna-hello-gui`, com o que o explorador precisa (o padrão de GUI dos apps).
- [ ] **v3:** `progress.*` no `shell.sock` (protocolo `1.1`), para o progresso das operações aparecer na central do shell.
