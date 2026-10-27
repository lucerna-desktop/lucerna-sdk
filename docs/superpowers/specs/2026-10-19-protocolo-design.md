# Protocolo do Lucerna: apps, serviço, shell e linha de comando

**Data:** 2026-10-19
**Base:** `docs/ecossistema.md` (as decisões de lá continuam valendo; este documento fecha o "formato exato do protocolo e do manifesto" e o "versionamento" que estavam em aberto).
**Atualização (2026-10-27):** o serviço sem systemd (subida pelo cliente, retomada pelo próximo cliente e trabalho agendado sem os timers do systemd), pelo princípio "Cada app funciona sozinho" do `ecossistema.md`. O código ainda não faz isso; ver "Serviço sem systemd".

## Por quê

Todos os apps do Lucerna (`lucerna-notes`, `lucerna-routine`, `lucerna-phone` e o `lucernad`) têm a mesma forma: um **serviço** sem tela que guarda os dados e faz o trabalho, e clientes que conversam com ele — o **painel** no shell, a **TUI** e, agora, uma **linha de comando**. Nenhum cliente mexe nos dados direto. Para isso funcionar igual em todos, os apps precisam de um jeito comum de conversar, de se anunciar ao shell e de subir sob demanda. É isso que o `lucerna-sdk` entrega, e nada de app concreto começa antes dele.

## Objetivos

- Uma forma única de conversar entre clientes e serviço, fácil de depurar à mão (`socat`, `jq`).
- O painel recebe o estado sem perguntar de novo (o serviço manda quando muda).
- O app pede coisas ao shell (barra, não perturbe, abrir a TUI) sem conhecer o shell por dentro.
- O shell descobre os apps instalados e carrega o painel certo, recusando com aviso o que não for compatível.
- O serviço sobe quando alguém conecta e sai sozinho quando ninguém usa, com ou sem systemd.
- Um app ganha a linha de comando de graça (`lucerna-routine add "estudar amanhã 19h"`), pelo mesmo protocolo.
- Um app de exemplo mínimo prova tudo isso de ponta a ponta.

## Não objetivos

- Rede: tudo é local, num computador, por socket Unix. A conversa celular ↔ computador do `lucerna-phone` é outro protocolo, do próprio app.
- Sincronização entre aparelhos (futuro, ver `ecossistema.md`).
- Autenticação entre processos do mesmo usuário: o socket fica numa pasta do usuário com permissão `0700`; quem consegue abrir o socket é o próprio usuário.
- Janela gráfica própria dos apps: a interface é o painel (no shell) ou a TUI.

## Anti-objetivos

- Um protocolo "esperto" que esconda o que trafega: tem de dar para ler as mensagens no terminal.
- O shell precisar de código específico de cada app (fora o painel QML que o próprio app traz).
- Um app que só funciona com o shell: sem o shell, a TUI e a linha de comando continuam completas.
- Um app que só funciona com systemd: sem ele, o cliente sobe o serviço e o trabalho agendado tem outro caminho (ver "Serviço sem systemd").

## Conversa: JSON-RPC 2.0 sobre socket Unix

- **Transporte:** socket Unix de fluxo (`SOCK_STREAM`) em `$XDG_RUNTIME_DIR/lucerna/<app>.sock` (pasta com permissão `0700`).
- **Enquadramento:** uma mensagem JSON por linha (UTF-8, terminada em `\n`, sem `\n` dentro da mensagem). Mensagens maiores que 1 MiB são recusadas (erro e conexão fechada) — escolha do construtor do documento, ajustável.
- **Formato:** JSON-RPC 2.0, como está na especificação: pedidos com `id`, respostas com `result` ou `error`, e notificações (sem `id`) nos dois sentidos.
- **Nomes de métodos:** `grupo.acao`, em inglês e minúsculas (`tasks.complete`, `notes.create`, `focus.start`). Os métodos do próprio protocolo começam com `lucerna.` e são reservados.

### Aperto de mão

A primeira mensagem de qualquer cliente é `lucerna.hello`:

```json
{"jsonrpc":"2.0","id":1,"method":"lucerna.hello","params":{"protocol":"1.0","client":"panel","app":"routine"}}
```

- `protocol`: a versão do protocolo do cliente (`MAJOR.MINOR`).
- `client`: `panel`, `tui`, `cli` ou outro nome livre (só para registro e para o serviço saber quem está ali).

O serviço responde com a versão dele, o nome, a versão do app e os tópicos (comuns e contínuos) e métodos que oferece:

```json
{"jsonrpc":"2.0","id":1,"result":{"protocol":"1.0","app":"routine","version":"0.1.0","topics":["today","focus"],"streams":[],"methods":["tasks.add","tasks.complete","focus.start","focus.stop"]}}
```

Compatível = mesmo `MAJOR`. Com `MAJOR` diferente, o serviço responde com o erro `-32010` ("versão do protocolo incompatível") e fecha. O painel mostra esse aviso no lugar do conteúdo, em vez de quebrar. Qualquer outro método antes do `hello` recebe `-32011`.

### Pedidos e respostas

O cliente chama um método do app; o serviço responde com `result` (qualquer JSON) ou `error`:

```json
{"jsonrpc":"2.0","id":7,"method":"tasks.complete","params":{"id":"t_42"}}
{"jsonrpc":"2.0","id":7,"result":{"ok":true}}
```

### Assinaturas (estado enviado pelo serviço)

- `lucerna.subscribe {"topics":["today","focus"]}` — o serviço responde com o estado **inteiro** de cada tópico (um retrato), e passa a mandar a notificação `lucerna.state` a cada mudança:

```json
{"jsonrpc":"2.0","method":"lucerna.state","params":{"topic":"focus","seq":12,"data":{"running":true,"mode":"pomodoro","remaining":1325,"tag":"estudo"}}}
```

- Cada mudança manda o retrato inteiro do tópico (não diferenças). Os tópicos são pensados pequenos (o que um painel mostra); um tópico grande é sinal de que deveria ser dividido. `seq` cresce por tópico, para o cliente descartar uma mensagem fora de ordem. Ele começa em `0` (o estado inicial) e volta a `0` quando o serviço reinicia; por isso um cliente que reconecta zera o último `seq` que viu (decisão do plano de implementação).
- **Tópicos contínuos** (decisão do construtor do documento): um tópico pode ser marcado como contínuo no `hello` (`"streams":["spectrum"]`), para dados de muitos quadros por segundo, como o espectro de áudio. Para eles vale "só o último": se um cliente está lento, o servidor descarta os quadros intermediários daquele cliente e manda o mais novo quando ele puder ler; nunca acumula fila sem limite e nunca desconecta por isso. Tópicos comuns também são coalescidos por cliente (se dois retratos do mesmo tópico estão esperando, só o mais novo vai). O serviço só produz um tópico contínuo enquanto houver alguém assinando.
- Um tópico comum também conta os assinantes (`Topic::subscribers`, `wait_subscribed`, `wait_unsubscribed` no `lucerna-ipc`): o serviço pode produzir um tópico comum só enquanto houver alguém assinando.
- `lucerna.unsubscribe {"topics":[...]}` para de mandar. Fechar a conexão desfaz tudo.
- Um contador que muda sozinho a cada segundo (o tempo restante do foco) **não** é enviado a cada segundo: o serviço manda o instante de fim (`ends_at`, horário Unix em segundos) e o cliente conta sozinho; só volta a mandar quando algo muda de verdade. (Regra para os apps; o exemplo acima é ilustrativo.)

### Erros

Códigos do JSON-RPC (`-32700` a `-32603`) como na especificação, mais os do protocolo:

| Código | Quando |
| --- | --- |
| `-32010` | versão do protocolo incompatível |
| `-32011` | método antes do `lucerna.hello` |
| `-32012` | tópico desconhecido |
| `-32000` a `-32009` | reservados ao protocolo |
| `1` em diante | erros do próprio app |

`message` sempre em português, pronta para mostrar ao usuário; `data` opcional com detalhes técnicos.

## Pedidos ao shell: o socket do shell

O shell também atende um socket, `$XDG_RUNTIME_DIR/lucerna/shell.sock`, no **mesmo** protocolo (aperto de mão com `client` = o nome do app e `app` = `"shell"`; o shell responde com `"app":"shell"`, e por isso `shell` é um id reservado, que nenhum app pode usar):

```json
{"jsonrpc":"2.0","id":1,"method":"lucerna.hello","params":{"protocol":"1.0","client":"routine","app":"shell"}}
```

É por ele que o app pede coisas ao shell. Métodos da versão 1:

| Método | O que faz |
| --- | --- |
| `bar.set {id, icon, text?, ends_at?, panel?}` | põe (ou atualiza) um item na barra: ícone, um texto curto (um número) e, opcional, um timer contado pelo shell até `ends_at`; `panel` diz se o clique abre o painel do app |
| `bar.clear {id}` | tira o item |
| `dnd.request {reason}` / `dnd.release` | pede para ligar / devolve o "não perturbe". O shell decide, mostra quem ligou (o nome do app e o `reason`) e desliga quando todos os que pediram devolveram (ou quando a conexão do app fecha) |
| `tui.open {args?}` | abre o terminal do usuário com a TUI do app, já com os argumentos (a nota, a tarefa) — sempre para o usuário; a comunicação nunca depende disso. O shell roda `<app.tui> tui <args…>` (o `app.tui` do manifesto, o subcomando oculto `tui` e cada argumento como um argumento separado, sem passar por shell); sem `args`, só `<app.tui> tui` |
| `panel.open {page?}` | abre o painel do app no shell, opcionalmente numa página |

- Os itens da barra e o pedido de não perturbe **pertencem à conexão**: se o serviço do app sai (ociosidade, queda), o shell os desfaz sozinho. Por isso um serviço com um item na barra (o pomodoro rodando) ou com um não perturbe pedido não sai por ociosidade enquanto ele existir: o SDK conta os dois como trabalho pendente (decisão da revisão final: o não perturbe some com a saída do serviço tanto quanto a barra).
- O shell não está rodando (o app usado só pela TUI): a conexão ao `shell.sock` falha; o SDK trata isso como "sem shell" e os pedidos viram nada, sem erro para o usuário.
- **Reconexão** (decisão do construtor do documento): o SDK guarda o estado desejado do app junto ao shell (os itens da barra e o pedido de não perturbe ativos). Se o shell não existe, cai ou reinicia (o Quickshell recarrega a config com frequência), o SDK tenta reconectar com espera crescente (até ~5 s entre tentativas) e, ao conseguir, refaz o aperto de mão e reaplica tudo. Para o app, a barra e o não perturbe são só "o que eu quero agora"; o SDK cuida de o shell refletir isso. Um item na barra (ou o não perturbe pedido) conta como trabalho pendente mesmo enquanto o shell está fora.
- **Pedido recusado** (decisão do construtor do SDK): se o shell responde a um pedido com erro, o SDK registra no stderr e não repete o pedido até o app mudar aquele item (ou o não perturbe) ou o shell reconectar. Uma conexão que cai logo depois do aperto de mão, sem responder pedido nenhum, não zera a espera crescente (a menos que tenha durado 1 s), para um shell quebrado não receber uma conexão a cada 100 ms.
- **`dnd.request` repetido** na mesma conexão só atualiza o `reason` (é idempotente): o shell não conta pedidos, e basta um `dnd.release` para devolver.
- **Notificações** não passam por aqui: o app usa o servidor de notificações do sistema (D-Bus `org.freedesktop.Notifications`), que o shell já atende.

## Manifesto

Um arquivo **JSON** por app em `/usr/share/lucerna/apps/<app>.json` (instalado pelo pacote). O shell também lê `$XDG_DATA_HOME/lucerna/apps/` (instalação local do usuário; vence a do sistema). JSON, e não TOML (decisão do construtor do documento), porque quem lê o manifesto é o shell, em QML, que tem `JSON.parse` e não tem leitor de TOML; assim o shell não precisa de nenhum programa auxiliar. O crate `lucerna-manifest` gera e valida o arquivo do lado dos apps, e a regra de compatibilidade é simples o bastante para ser repetida no shell (com um caso de teste comum aos dois lados, num arquivo de exemplos versionado neste repositório: `crates/lucerna-manifest/testdata/manifest-cases.json`, dentro do crate para ir junto no pacote publicado).

```json
{
  "schema": 1,
  "app": {
    "id": "routine",
    "name": "Rotina",
    "icon": "event_available",
    "socket": "routine.sock",
    "tui": "lucerna-routine"
  },
  "panel": { "qml": "/usr/share/lucerna/panels/routine/Panel.qml", "api": "1.0" },
  "card": { "qml": "/usr/share/lucerna/panels/routine/Card.qml" },
  "shortcuts": { "panel": "SUPER+R" }
}
```

- `app.id`: o mesmo nome do socket e das pastas. `app.icon`: nome de ícone Material Symbols, ou caminho de um SVG. `app.socket`: relativo a `$XDG_RUNTIME_DIR/lucerna/`. `app.tui`: o comando da TUI (para `tui.open`).
- `panel` e `card` são opcionais (um app só de serviço, como o `lucernad`, não tem painel). `shortcuts` são sugestões; o usuário decide nas Configurações.
- `schema`: a versão do formato do manifesto. O shell ignora, com aviso no log, um manifesto de `schema` que não conhece.
- `panel.api`: o shell carrega o painel se o `MAJOR` bate e o `MINOR` do painel é menor ou igual ao do shell. Senão, o painel não aparece e as Configurações mostram o aviso ("Rotina precisa de uma versão mais nova do Lucerna").

## Três versões, cada uma no seu lugar

| O quê | Formato | Onde | Muda quando |
| --- | --- | --- | --- |
| Protocolo | `MAJOR.MINOR` | `lucerna.hello` | `MINOR`: algo novo e opcional; `MAJOR`: quebra |
| Manifesto | inteiro (`schema`) | o JSON | o formato muda |
| API de componentes do shell | `MAJOR.MINOR` | `[panel].api` | o mesmo critério do protocolo |

Os crates do SDK seguem semver próprio; a versão do protocolo que cada versão do crate fala está documentada no crate.

## Serviço sob demanda

- **Ativação:** cada app instala `lucerna-<app>.socket` e `lucerna-<app>.service` do systemd do usuário. O socket fica em `%t/lucerna/<app>.sock`; o systemd passa o socket ao serviço (`LISTEN_FDS`). O SDK traz o código para receber esse socket — e, para desenvolvimento e para quando não há systemd, para criar o socket sozinho quando o serviço é rodado à mão.
- **Saída por ociosidade:** o serviço sai depois de **60 s** sem clientes conectados e sem trabalho pendente (valor padrão do SDK, configurável por app). "Trabalho pendente" é decidido pelo app (um timer de foco rodando, um item na barra, uma transferência do celular). O systemd sobe de novo na próxima conexão; sem systemd, o próximo cliente o sobe (ver "Serviço sem systemd").
- **Queda:** com systemd, a unidade tem `Restart=on-failure`. Sem systemd, ninguém reinicia: o próximo cliente que conectar sobe o serviço de novo, pelo mesmo caminho da subida.
- **Trabalho agendado:** o SDK registra um timer do systemd do usuário que sobe o serviço no horário; o serviço faz o trabalho (o lembrete), cancela o timer e volta a sair. Decisão do construtor: um arquivo de unidade `lucerna-<app>_<nome>.timer` (o `_` separa o id do app do nome do timer, que não o contêm, e evita colisão entre, por exemplo, o app `a-b` com o timer `c` e o app `a` com o timer `b-c`), com `OnCalendar` em UTC e `Unit=lucerna-<app>.service`, ligado com `systemctl --user`. Persistente (sobrevive a reinício): em `$XDG_CONFIG_HOME/systemd/user/` com `Persistent=true`, `enable` e `restart`; não persistente: em `$XDG_RUNTIME_DIR/systemd/user/` com `restart`. O `lucerna-<app>.service` precisa estar instalado (o SDK confere antes e recusa); o horário precisa estar ao menos 2 s no futuro; trocar a persistência com o mesmo nome apaga o arquivo da outra pasta; uma falha no meio desfaz o que foi feito. Sem o `systemctl --user` ou sem a unidade instalada, o trabalho agendado vai pelo agendador local (ver "Serviço sem systemd"), em vez de ser recusado.

## Serviço sem systemd

Vale para todo app e para o `lucernad`, pelo princípio "Cada app funciona sozinho" do `ecossistema.md`. O protocolo não muda: o que muda é quem sobe o serviço e quem o acorda no horário.

**Subida pelo cliente** (o molde é o do spec do explorador, no `lucerna-files`):

1. O cliente (linha de comando, TUI ou GUI) conecta ao `<app>.sock`. Com a unidade do systemd instalada, a própria conexão sobe o serviço, como antes.
2. Sem ninguém ouvindo (o socket não existe ou recusa a conexão: sem systemd, unidade não instalada, desenvolvimento), o cliente roda `lucerna-<app> serve` solto: o mesmo binário (o executável atual), num grupo de sessão novo, com a entrada vazia e a saída num log em `$XDG_STATE_HOME/lucerna/<app>/serve.log`. Depois tenta conectar por até **2 s**, com espera crescente. Dois clientes fazendo isso juntos: o segundo `serve` não pega o lock do socket e sai (`AlreadyRunning`, que já existe no SDK), e os dois clientes conectam no primeiro.
3. Sem conseguir em 2 s: a linha de comando sai com o código `2` ("sem serviço") e diz o porquê, com o caminho do log; a TUI e a GUI mostram a causa e seguem com o que não depende do serviço, se houver.

O serviço subido assim cria o socket à mão (o caminho de desenvolvimento que já existe), sai por ociosidade do mesmo jeito e apaga o socket ao sair.

**Retomada:** sem systemd, uma queda só é notada pelo próximo cliente, que sobe o serviço de novo (passo 2). O app guarda o que precisa sobreviver a uma queda nos dados dele; ao subir, o serviço confere o que estava em andamento ou venceu.

**O painel** é um cliente como os outros: com a conexão recusada, o host de painéis do shell roda `lucerna-<app> serve` pelo mesmo caminho (o binário é o `app.tui` do manifesto, que é o próprio `lucerna-<app>`) e reconecta com a mesma espera. Isso é do lado do shell.

**Trabalho agendado sem os timers do systemd: o agendador local.**

- O SDK escolhe o agendador ao subir: o do systemd quando o `systemctl --user` responde e a unidade `lucerna-<app>.service` carrega; senão, o **agendador local**. A interface (`Scheduler`: `schedule` e `cancel`) é a mesma, e o app não precisa saber qual está em uso.
- O agendador local guarda os horários no próprio serviço e, no horário, avisa o app pelo mesmo caminho do timer do systemd (o app faz o trabalho vencido e chama `cancel`). **Cada timer marcado conta como trabalho pendente**, então o serviço não sai por ociosidade enquanto houver algo agendado: ele fica rodando até o próximo horário.
- O app guarda o que agendou nos próprios dados (a rotina já guarda os lembretes no banco). Ao subir, por qualquer caminho, o serviço confere o que venceu enquanto estava parado, faz na hora e reagenda o resto. Isso faz o papel do `Persistent=true`.
- **Limite:** sem systemd, depois de reiniciar o computador ou sair da sessão, nada sobe o serviço sozinho; o trabalho agendado só acontece quando algo o subir (um cliente, por exemplo). Para cobrir o login, o agendador local, enquanto houver timer **persistente** marcado, mantém uma entrada de início automático do XDG em `$XDG_CONFIG_HOME/autostart/lucerna-<app>.desktop` (`Exec=lucerna-<app> serve`, `NoDisplay=true`) e a apaga quando não há mais nenhum; os timers não persistentes não a criam. O KDE, o GNOME e o XFCE leem essa pasta; um compositor que não lê (o Hyprland sem systemd, por exemplo) precisa de uma linha como `exec-once = lucerna-<app> serve`, e o app diz isso ao usuário.
- O serviço subido no login sem nada vencido nem agendado sai por ociosidade, como sempre.

## Linha de comando e binários

- **Um binário por app** (decisão do construtor do documento): `lucerna-<app>` é tudo. Sem argumentos, abre a TUI; com um subcomando, vira cliente da linha de comando; com o subcomando oculto `serve`, é o serviço (o `ExecStart` da unidade do systemd é `lucerna-<app> serve`); com o subcomando oculto `tui <args…>`, abre a TUI com esses argumentos (é o que o shell roda no `tui.open`; depois do primeiro argumento, tudo vai para a TUI, mesmo o que começa com `-`). `lucerna-<app>` sem nada é o mesmo que `lucerna-<app> tui` sem argumentos (decisão da revisão final: um subcomando explícito evita que um argumento da TUI, como `t_42`, seja lido como um subcomando desconhecido). O `lucerna-cli` e o `lucerna-service` se compõem num `main` só, e o `lucerna-hello` mostra esse molde, que todos os apps copiam.
- O SDK traz o esqueleto da linha de comando: com um subcomando, o binário conecta, faz o aperto de mão (`client` = `cli`), chama o método e imprime o resultado.
- Os subcomandos são declarados pelo app (nome, ajuda em português, argumentos) e cada um vira um método do protocolo. Ex.: `lucerna-routine add "estudar amanhã 19h"` → `tasks.add {"text":"estudar amanhã 19h"}`; `lucerna-routine focus start --tag estudo` → `focus.start {...}`.
- Saída legível por padrão; `--json` imprime o `result` cru (para scripts). Código de saída `0` no sucesso, `1` com erro do app, `2` sem conseguir falar com o serviço (nem subi-lo em 2 s, ver "Serviço sem systemd") e `64` com uso errado da linha de comando (decisão do plano de implementação: o `2` padrão do clap colidiria com "sem serviço").
- Subcomandos comuns que todo app ganha: `status` (o retrato dos tópicos) e `watch <tópico>` (acompanha as mudanças no terminal, útil para depurar). `status`, `watch`, `serve` e `tui` são nomes reservados: um app não declara subcomando com eles.

## Crates do `lucerna-sdk`

Um workspace Cargo (edição 2024, licença GPL-3.0), publicado no crates.io:

| Crate | Conteúdo |
| --- | --- |
| `lucerna-protocol` | os tipos das mensagens (JSON-RPC, `hello`, `state`, erros), a versão e os nomes reservados; sem I/O |
| `lucerna-ipc` | cliente e servidor do socket (assíncronos), o aperto de mão, as assinaturas com retrato e `seq`, o limite de tamanho, a subida do serviço pelo cliente sem systemd |
| `lucerna-service` | o esqueleto do serviço: socket do systemd (ou criado à mão), saída por ociosidade, trabalho pendente, timers agendados (do systemd ou o agendador local), a conexão ao `shell.sock` (barra, não perturbe, `tui.open`, `panel.open`) |
| `lucerna-cli` | o esqueleto da linha de comando (subcomandos → métodos, `status`, `watch`, `--json`, códigos de saída) |
| `lucerna-paths` | as pastas de "Pastas e dados" do `ecossistema.md`, mais a de estado (`$XDG_STATE_HOME`, para o log do serviço subido pelo cliente) |
| `lucerna-manifest` | os tipos do manifesto, a leitura, a validação e a regra de compatibilidade de `api` |

E o app de exemplo **`lucerna-hello`** (no mesmo repositório, não publicado): um contador com um tópico comum (`counter`) e um contínuo (`ticks`, para exercitar o "só o último"), três métodos (`counter.add`, `counter.reset` e `quiet.set`, que liga e desliga o não perturbe, para o teste do `shell.sock`; o terceiro é decisão do plano de implementação), um item na barra com o valor, a linha de comando (`lucerna-hello add 3`, `status`, `watch counter`), as unidades do systemd e o manifesto **sem painel**. O painel QML do `lucerna-hello` fica para o spec do lado do shell, quando a API de componentes `1.0` existir (decisão do construtor do documento: escrever o painel agora seria contra uma API que ainda não foi definida).

## Lado do shell (no `lucerna-shell`, fora deste repositório)

Este documento define o contrato; a implementação no shell é um projeto do `lucerna-shell`, com spec próprio, que depende deste. O que o contrato exige do shell:

- um cliente QML do protocolo (um `Socket` do `Quickshell.Io` com o aperto de mão e as assinaturas), exposto aos painéis dos apps;
- o servidor do `shell.sock` com os métodos da tabela acima;
- o host de painéis: ler os manifestos, checar `schema` e `api`, carregar `Panel.qml`/`Card.qml`, registrar os atalhos sugeridos nas Configurações;
- com a conexão ao serviço recusada, subir o serviço (`lucerna-<app> serve`) como os outros clientes (ver "Serviço sem systemd");
- a API de componentes versionada (`1.0`), com os widgets e o tema do Lucerna.

A ordem combinada: o SDK primeiro (com o `lucerna-hello` testável sem shell, pela linha de comando), depois o espectro do `lucernad` como primeiro uso real, e o lado do shell quando o primeiro painel de app for necessário.

O `lucernad` segue o mesmo modelo (serviço por socket do systemd ou subido pelo cliente, saída por ociosidade, tópicos, manifesto sem painel): o shell é o cliente dele, e o espectro é um tópico contínuo, produzido só enquanto o shell assina. A forma do `lucernad` (um binário com módulos ou vários) continua em aberto no `ecossistema.md`, para o spec próprio dele.

## Testes

- Unidade em cada crate (serialização das mensagens, a regra de compatibilidade de versão, o enquadramento por linha com mensagens quebradas e grandes, os caminhos XDG, a validação do manifesto).
- Integração: o `lucerna-hello` de ponta a ponta num socket temporário — aperto de mão, versão incompatível, pedido, assinatura com retrato e mudanças, dois clientes ao mesmo tempo, um cliente lento num tópico contínuo (recebe só o último, sem fila crescer), saída por ociosidade, a linha de comando com `--json` e os códigos de saída, o `shell.sock` simulado (barra e não perturbe desfeitos quando a conexão fecha, reaplicados quando o shell volta).
- Sem systemd: a linha de comando sobe o `lucerna-hello serve` quando não há socket; dois clientes ao mesmo tempo acabam num serviço só; sem conseguir em 2 s, código `2`; depois de matar o serviço, o próximo cliente o sobe de novo; o agendador local dispara no horário, segura a ociosidade enquanto há timer, cria e apaga a entrada de início automático só para os persistentes, e o trabalho vencido com o serviço parado sai ao subir. O CI roda tudo isso sem systemd de usuário.
- CI no GitHub com os mesmos critérios do `lucerna-shell`: `fmt`, `clippy` sem avisos, testes, e checks obrigatórios no ruleset quando o CI existir.

## O que fica para o construtor

- As bibliotecas (runtime assíncrono, JSON, CLI), desde que caibam na licença GPL-3.0.
- O formato exato dos argumentos de `bar.set` além dos campos listados, e a forma do timer agendado do systemd (desde que sobreviva a reinício quando pedido).
- O limite de 1 MiB e os 60 s de ociosidade são escolhas deste documento (não do usuário), ajustáveis com motivo.
