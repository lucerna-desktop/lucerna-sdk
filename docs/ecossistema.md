# Ecossistema Lucerna

Este documento descreve como os repositórios em Rust se encaixam no [Lucerna](https://github.com/lucerna-desktop/lucerna-shell). Ele só registra o que já foi decidido; o que falta definir está em "Em aberto".

## Cada app funciona sozinho

**Todo app do Lucerna se instala e funciona sozinho.** Uma pessoa pode instalar e usar qualquer um deles sem instalar o resto: sem o shell, sem o `lucernad`, sem o Hyprland, com ou sem systemd, em qualquer distribuição e ambiente (KDE, GNOME e outros). A integração com o shell é um extra, que liga quando o shell está presente. O `lucerna-sdk` entra só como biblioteca, na compilação, e não é algo a instalar.

O que isso quer dizer para cada parte:

- **Serviço:** sobe e trabalha do mesmo jeito sem o shell. Sem o `shell.sock`, os pedidos ao shell (barra, não perturbe, `tui.open`, `panel.open`) viram nada, sem erro e sem espera, e são reaplicados se o shell aparecer depois. Sem systemd, o cliente (TUI, CLI ou GUI) sobe o serviço sozinho, e o trabalho agendado precisa de um caminho que não dependa dos timers do systemd. Avisos vão pelo servidor de notificações do sistema (`org.freedesktop.Notifications`), que o shell atende e que o KDE e o GNOME também atendem.
- **TUI e linha de comando:** são a interface completa do app. Tudo o que o painel faz também se faz pela TUI; nenhuma ação existe só no painel.
- **Painel:** só existe dentro do shell, e é um atalho. Sem o shell, não há painel e nada se perde.
- **GUI** (quando o app tiver): roda em qualquer ambiente, sem depender do KDE, do GNOME nem do Hyprland.
- **Tema:** sem o shell, a TUI usa a paleta do terminal e a GUI usa o tema padrão do Lucerna, embutido no binário. Com o shell, os dois podem acompanhar o tema dele.
- **Pacote:** não depende do `lucerna-shell` nem do `lucernad`. O manifesto e o painel são só arquivos que o shell lê quando existe.

## Repositórios

Um repositório por produto, mais o `lucerna-sdk` como base comum. Todos têm as mesmas configurações do `lucerna-shell`: licença GPL-3.0, branch `main` protegida por ruleset, merge só por merge commit, PR obrigatório e padrão de commits em português.

| Repositório | Papel |
|---|---|
| [`lucerna-shell`](https://github.com/lucerna-desktop/lucerna-shell) | O shell, em Quickshell (QML). Só a interface. |
| [`lucerna-sdk`](https://github.com/lucerna-desktop/lucerna-sdk) | Base comum: o protocolo, o esqueleto do serviço e da linha de comando, os caminhos e o manifesto dos apps. |
| [`lucernad`](https://github.com/lucerna-desktop/lucernad) | Daemon com a lógica do shell (espectro, sistema, área de transferência e mais). |
| [`lucerna-phone`](https://github.com/lucerna-desktop/lucerna-phone) | Gerenciador de celulares. |
| [`lucerna-phone-android`](https://github.com/lucerna-desktop/lucerna-phone-android) | App Android do `lucerna-phone`. |
| [`lucerna-routine`](https://github.com/lucerna-desktop/lucerna-routine) | Gerenciador de rotina. |
| [`lucerna-notes`](https://github.com/lucerna-desktop/lucerna-notes) | Bloco de notas. |

## O shell é só a interface

O `lucerna-shell` (Quickshell/QML) cuida da interface: painéis, temas, animações e as APIs nativas do Quickshell. A lógica fica em Rust, nos outros repositórios.

## Estrutura de cada app

Cada app tem quatro partes, todas no repositório dele:

1. **Núcleo:** uma biblioteca com a lógica.
2. **Serviço:** um processo sem tela que guarda o estado, faz o trabalho e atende o socket. É ele que conversa com o shell.
3. **TUI:** uma interface de terminal, feita com ratatui.
4. **Painel:** o próprio painel, em QML.

A TUI e o painel são clientes do serviço. Nenhum dos dois mexe nos dados direto.

O pacote do app instala o painel e um manifesto numa pasta que o shell conhece, por exemplo `/usr/share/lucerna/apps/*.json`.

## Host de painéis no shell

O shell terá um "host de painéis":

- lê os manifestos e carrega os painéis;
- oferece uma API de componentes QML estável e versionada, com os widgets e o tema do Lucerna;
- confere no manifesto para qual versão da API o painel foi feito e recusa, com um aviso, o que não consegue carregar.

Sem o app instalado, o painel não existe.

Os painéis dos apps abrem como os de hoje, por atalho, IPC ou ícone na barra, e seguem a regra de fechar os outros painéis. Isto é uma suposição aceita, ainda sem implementação.

## Protocolo

Definido em [`superpowers/specs/2026-10-19-protocolo-design.md`](superpowers/specs/2026-10-19-protocolo-design.md). Em resumo:

- um socket Unix por app, em `$XDG_RUNTIME_DIR/lucerna/<app>.sock`, com JSON-RPC 2.0, uma mensagem por linha;
- aperto de mão (`lucerna.hello`) com a versão do protocolo (`MAJOR.MINOR`; compatível com o mesmo `MAJOR`);
- tópicos de estado que o serviço manda inteiros a cada mudança (`lucerna.subscribe` e `lucerna.state`), e tópicos contínuos para dados rápidos, como o espectro;
- o socket do shell (`shell.sock`), no mesmo protocolo, para os pedidos do app: barra, não perturbe, abrir a TUI e abrir o painel;
- o manifesto JSON de cada app (`schema` 1), com a versão da API de componentes do painel;
- um binário por app, `lucerna-<app>`: a TUI, a linha de comando e, com `serve`, o serviço.

O `lucerna-sdk` traz os crates disso tudo (`lucerna-protocol`, `lucerna-ipc`, `lucerna-service`, `lucerna-cli`, `lucerna-paths` e `lucerna-manifest`), publicados no crates.io, e o app de exemplo `lucerna-hello`.

## Serviço sob demanda

O serviço é a única parte que mexe nos dados do app. Isso vale para o painel e para a TUI:

- **Os dois são clientes do serviço.** Como ninguém mexe nos dados por fora, os dois nunca brigam pelo mesmo dado, e o que muda num aparece na hora no outro.
- **A TUI é separada do painel e é uma alternativa a ele.** O shell nunca abre a TUI para conversar com o app.
- **O painel abre o app sozinho, sem TUI.** Ele conecta ao socket, e o serviço sobe por ativação de socket do systemd do usuário (`lucerna-<app>.socket` + `lucerna-<app>.service`). Se o serviço não estiver rodando, a TUI o sobe pelo mesmo caminho.
- **O serviço não fica rodando o tempo todo.** Ele sobe quando um cliente conecta, sai sozinho depois de um tempo sem clientes e sem trabalho pendente e, se cair, o systemd reinicia.
- **Sem systemd, o app continua funcionando** (ver "Cada app funciona sozinho"): o cliente sobe o serviço e o próximo cliente o sobe de novo depois de uma queda.

O serviço sai depois de 60 s sem clientes e sem trabalho pendente (um item na barra ou o não perturbe pedido contam como trabalho); o valor é ajustável por app.

**Trabalho agendado sem o serviço rodando:** um lembrete da rotina, por exemplo, precisa acontecer com o serviço parado. Então o serviço registra um timer do systemd do usuário para o próximo horário (`lucerna-<app>_<nome>.timer`, persistente ou não). O timer sobe o serviço, que avisa pelas notificações do shell, cancela o timer e volta a sair. Os detalhes estão no spec do protocolo.

## Painel e TUI

O painel no shell serve para **olhar rápido e agir rápido**, sem tirar o usuário do que está fazendo. A TUI é o lugar do **trabalho completo**: editar, organizar e configurar.

## Pastas e dados

Por enquanto, os dados ficam só neste computador. As pastas seguem o padrão XDG:

| Tipo | Onde | Exemplo |
|---|---|---|
| Documentos do usuário (o que ele abre e edita) | pasta visível e configurável | `~/Documentos/Notas` |
| Dados internos | `~/.local/share/lucerna/<app>/` (`$XDG_DATA_HOME`) | banco da rotina, chaves de pareamento do celular |
| Configuração | `~/.config/lucerna/<app>.toml` (`$XDG_CONFIG_HOME`) | preferências |
| Cache (pode ser apagado e se refaz) | `~/.cache/lucerna/<app>/` (`$XDG_CACHE_HOME`) | índice de busca das notas, miniaturas |
| Sockets | `$XDG_RUNTIME_DIR/lucerna/<app>.sock` | painel e TUI |

- Cada app só mexe na própria subpasta.
- O nome `lucerna` vale mesmo quando o app roda só na TUI.
- O `lucerna-sdk` traz essas regras de caminho num crate pequeno, para os apps não divergirem.

## O que o SDK oferece

Estas capacidades vêm dos casos dos apps:

1. **Estado enviado pelo app:** a bateria do celular, a próxima tarefa, as notas recentes. O painel só mostra.
2. **Ações chamadas pelo painel:** "marcar tarefa", "salvar nota", "espelhar", com resposta de sucesso ou erro.
3. **Itens na barra:** um ícone com um número ou um timer que o app põe na barra do shell (o pomodoro, o celular conectado).
4. **Notificações:** pelo servidor de notificações do sistema (`org.freedesktop.Notifications`), que o shell já atende, sem nada novo. Funciona igual no KDE, no GNOME e em outros ambientes.
5. **Abrir na TUI:** o painel pode pedir ao shell para abrir o terminal já no app, na nota ou tarefa certa, **para o usuário**. É um atalho para o trabalho completo. A comunicação com o app nunca depende da TUI.
6. **Dois tamanhos:** o painel do app, que abre como os outros painéis, e, opcional, um card pequeno na aba Painel do dashboard do shell.
7. **Caminhos:** um crate pequeno com as regras de pastas de "Pastas e dados" (dados, configuração, cache e sockets), para os apps não divergirem.
8. **Controle do "não perturbe" do shell:** um app pode pedir para ligar e desligar o não perturbe, como o foco da rotina faz. O shell decide e mostra quem ligou.

## Em aberto

O protocolo, o manifesto, o versionamento, a ociosidade, os timers e o esqueleto do serviço foram fechados no spec do protocolo. Continua em aberto:

- O empacotamento, incluindo o AUR.
- A forma do `lucernad`: um daemon com módulos ou vários binários.
- Como o cliente sobe o serviço sem systemd (no `lucerna-ipc`) e como o trabalho agendado acontece sem os timers do systemd.

## Futuro

- A sincronização dos dados entre aparelhos. Por enquanto, os dados ficam só neste computador.
