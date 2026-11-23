# Motor de loops multi-agente (`tchelvis loop`) — design

**Data:** 2026-11-23 · **Status:** aprovado em 2026-11-23 · **Tamanho:** projeto (M11)

Revisa a especificação enviada pelo usuário em 2026-11-23. Onde este documento difere dela, a seção "Mudanças" diz por quê.

## Intenção

Orquestradores queimam contexto e perdem qualidade ao gerenciar cada turno entre agentes de código. O `tchelvis loop` executa uma topologia fixa (ex.: worker → reviewer → critic) em sessões limpas, que trocam artefatos só por arquivo. O agente que pediu **encerra o turno** e é acordado **uma única vez**, quando o loop termina, trava ou precisa de uma pessoa.

## Objetivos, não objetivos e antiobjetivos

**Objetivos:** execução determinística; no máximo 5 linhas para quem chamou; histórico completo em disco; telemetria por etapa; retomada após queda.

**Não objetivos:** merge automático; decidir a topologia com LLM (isso é o `orch`); rodar em outra máquina.

**Seria um fracasso mesmo funcionando se:** código do agente executasse fora do sandbox; o loop rodasse sem sandbox em silêncio; o agente fosse acordado a cada etapa; trabalho do usuário fosse sobrescrito.

## Decisões do usuário

1. Etapas headless, em **um único terminal Herdr** por loop.
2. O `tchelvis` vigia o loop e só devolve ao agente quando termina ou trava.
3. Sandbox com **Incus** ou outra ferramenta, não ai-jail.
4. Workflows em **TOML**.
5. Depois do M10.
6. Primeiro com o `tchelvis` **dentro do container** (`inherited`); depois, do host pelo Incus.

## Abordagens

| | Como | Troca |
|---|---|---|
| **A (escolhida)** | Processo `tchelvis loop` em um terminal Herdr; o daemon só vigia e avisa | Cair o daemon não derruba o loop; reutiliza a entrega do `orch` |
| B | Motor dentro do daemon | Sem terminal visível; um erro do loop atinge a frota |
| C | Cada etapa como um dispatch do `orch` | Reutiliza mais, mas são sessões interativas, sem JSON nem tokens |

## Estrutura

```text
<repo>/.loops/
├── workflows/*.toml
├── roles/*.md
└── runs/                     # com um .gitignore próprio contendo "*"
    └── <run-id>/             # state.json, input.md, artefatos, report.md, logs/
~/.config/tchelvis/loops/     # workflows e roles globais
~/.config/tchelvis/loops.toml # sandbox e limites: só aqui, nunca no repositório
<state>/tchelvis/loops/<run-id>/workspace/   # cópia onde os agentes trabalham
```

Workflows e roles são lidos do repositório principal no início, e o conteúdo usado é copiado para a run. Nunca são relidos do workspace, que os agentes podem editar. O `.gitignore` do projeto não é alterado.

## Workspace e git

Cada run tem um **clone separado** (`git clone --no-hardlinks`) no commit `HEAD`, e não um worktree: o `.git` de um worktree aponta para o repositório principal, que teria de ficar gravável para os agentes.

**Regra:** fora do sandbox, o `tchelvis` nunca executa git usando o `.git` do workspace. Verificado em 2026-11-23: um `git status` ali executou um hook e um `core.fsmonitor` plantados. O motor usa o repositório principal como `GIT_DIR`, o workspace como `GIT_WORK_TREE` e um índice próprio da run, com `core.hooksPath=/dev/null`, `core.fsmonitor=false`, `--no-ext-diff` e `--no-textconv`; no mesmo teste, nada foi executado.

- **Snapshot:** depois de cada etapa, `add -A` (sem `.tchelvis-loop/`) e `write-tree`. Independe de o agente ter feito commit e inclui arquivos novos, respeitando o `.gitignore`.
- **`current.diff`:** commit-base contra o último snapshot.
- **Etapas sem `edits = true`:** se a árvore mudar, a etapa falha e o workspace volta ao snapshot anterior.
- **Resultado:** `commit-tree` na branch `loop/<workflow>-<run-id>` do repositório principal, sem mexer no checkout. A branch nunca pode já existir; o `tchelvis` não faz merge nem push.
- **Repositório com mudanças não commitadas:** erro, a menos que se passe `--from-head` (elas ficam de fora).
- **Repositórios aninhados, submódulos alterados ou falha no `add`:** `ERROR` com o caminho, workspace preservado.

## Artefatos

Cada etapa vê `.tchelvis-loop/in/` (somente os `reads`) e grava em `.tchelvis-loop/out/`. O motor recria as duas pastas a cada etapa e recolhe apenas o arquivo de `writes`: regular, sem seguir links simbólicos em nenhum componente, até 1 MiB e em UTF-8. Um link para `~/.ssh/id_rsa` é recusado. Se o repositório já tiver `.tchelvis-loop`, o loop não inicia.

Versões por ciclo são preservadas (`c1-review.md`); `reads` recebe a mais recente. Um `reads` ainda não produzido é omitido no prompt.

## Sandbox

Configurado só em `~/.config/tchelvis/loops.toml`:

```toml
max_concurrent = 2
auto_approve_in_sandbox = true   # obrigatório: autoriza os CLIs a agir sem perguntar, só no sandbox
env_pass = []

[sandbox]
kind = "inherited"               # inherited | bwrap | incus

[sandbox.bwrap]
home_rw = [".claude", ".local/share/opencode"]
home_ro = [".cargo", ".rustup"]

[sandbox.incus]
instance = "dev"                 # instância já preparada, com CLIs, login e toolchain
user = "ubuntu"
```

| Ordem | Tipo | Como | Protege |
|---|---|---|---|
| 1 | `inherited` | O `tchelvis` já roda em um container; exige detecção e escolha explícita | O host, só na medida do container. **Nada dentro dele** |
| 2 | `bwrap` | `/` somente leitura, `$HOME` e `/run` vazios, só workspace e `home_rw` graváveis, PID próprio; funciona dentro de um container | Também o repositório principal, outros projetos, sockets e o Herdr |
| 3 | `incus` | `incus exec` na instância, a partir do host; o workspace precisa estar montado nela | O host |

- **Limite do `inherited`:** o agente tem os mesmos poderes do usuário do container. Se houver `sudo` sem senha, acesso ao Docker ou ao Incus, pastas do host graváveis ou `hc`, ele alcança tudo isso, e também o repositório principal, o `state.json` e os outros terminais. `loop check` lista o que encontrou, e cada relatório registra o tipo de sandbox usado. O git seguro continua valendo contra execução acidental, mas não é uma fronteira nesse modo.
- **Sem as opções de aprovação, os CLIs podem agir mesmo assim:** em teste de 2026-11-23, `opencode` e `agy` criaram arquivos sem nenhuma delas, por configuração do usuário. O sandbox é a única barreira.
- **Sonda antes da primeira etapa:** em `bwrap` e `incus`, o motor grava um valor aleatório no workspace e o lê dentro do sandbox, confirma que consegue escrever ali e que **não** lê um arquivo-sentinela criado fora dele. No `inherited`, confirma apenas que está em container. Falhou: o loop não inicia. Nunca há troca automática de tipo.
- **`bwrap` e `incus`:** o ambiente é limpo (passam `PATH`, `HOME`, `LANG`, `TERM` e `env_pass`) e não há sockets do Docker, do Herdr, do SSH ou do D-Bus. No `inherited`, o ambiente é limpo do mesmo jeito, mas os sockets continuam alcançáveis.
- **Nada pode rodar em serviço externo:** `opencode run` usa por padrão um serviço em segundo plano, que ficaria fora do sandbox; o adapter sempre usa `--standalone`. Todo adapter precisa passar por um teste real: pedir uma escrita fora do workspace, em `bwrap`, e confirmar que ela não acontece.
- **Limite comum:** a rede fica aberta para a API, e o agente enxerga a própria credencial.
- **`bwrap` não é total:** a rede é compartilhada, então serviços em `localhost` e sockets abstratos continuam alcançáveis.
- **Recomendado para uso normal: `bwrap` dentro do container.** Medido em 2026-11-23 (bubblewrap 0.9.0 em um container Incus sem privilégios): cerca de 8 ms a mais por processo iniciado, pagos uma vez por etapa; criação de 3.000 arquivos e `cargo check` sem perda mensurável. Escrita fora do workspace, leitura de um arquivo-sentinela, `sudo`, sockets do Docker e do Incus, `hc` e pasta compartilhada do host ficaram inacessíveis. O DNS exige montar `/run/systemd/resolve` somente leitura.
- `bwrap` exige o pacote `bubblewrap`. `incus` só pode ser validado no host.

## Adapters

Fase A: `claude`, `agy` e `opencode`. Cada um monta os argumentos, envia o prompt por stdin ou arquivo e lê um **fluxo de eventos JSON**. O fluxo é necessário porque o `json` simples do Claude só imprime no fim, e seria impossível distinguir trabalho de travamento.

- Cada etapa é uma sessão nova, sem retomar a anterior.
- As opções de aprovação automática de cada CLI só são usadas com a sonda aprovada e `auto_approve_in_sandbox = true`.
- Turnos, tokens e custo ausentes viram `null`; somas com `null` aparecem como parciais (`≥ 18.420`).
- Os formatos dos três CLIs foram capturados em 2026-11-23 (sucesso e modelo inválido) e viram fixtures. Um formato inesperado não falha a etapa, só deixa a telemetria `null`.
- `claude` usa `--setting-sources project`: plugins, hooks e instruções globais do usuário não entram nas etapas.
- O código de saída diferente de zero falha a etapa.
- `pi` e `codex`: fase B. `custom`: fase C, só em workflow global.

## Workflow

```toml
name = "worker-reviewer-critic"
description = "Implementa, revisa e audita antes de aprovar"
max_cycles = 3
stage_timeout_secs = 1800
stall_secs = 600
max_duration_secs = 7200

[[stages]]
id = "implement"
role = "worker"
provider = "opencode"
model = "zai/glm-5.3-flash"
reads = ["input.md", "review.md", "critic.md"]
edits = true
next = "review"

[[stages]]
id = "review"
role = "reviewer"
provider = "agy"
model = "gemini-4-hydrogen"
reads = ["input.md", "current.diff"]
writes = "review.md"
next = "audit"

[[stages]]
id = "audit"
role = "critic"
provider = "claude"
reads = ["input.md", "current.diff", "review.md"]
writes = "critic.md"
verdict_gate = true
on_pass = "done"
on_retry = "implement"
```

Destinos são sempre **ids de etapa** ou `done`. Chave desconhecida é erro. Antes de iniciar, o motor recusa: id repetido ou inválido; destino inexistente; etapa inalcançável; nenhum caminho até `done`; ciclo sem `verdict_gate`; gate sem `writes`; `next` junto com gate; nome de artefato com `/`, `..` ou reservado; `reads` que ninguém produz; provider fora da lista ou ausente no sandbox. Só o `on_pass` de um gate leva a `done`, e o grafo sem `on_retry` não pode ter ciclos, porque só `RETRY` conta como ciclo. Nomes de workflow e role usam `[a-z0-9-]`; `max_cycles` vai até 20 e os tempos, até 7 dias. `home_rw` e `home_ro` não podem alcançar os arquivos do próprio `tchelvis`. O modelo é opcional.

**Presets:** `worker-reviewer-critic`, `designer-worker-reviewer`, `planner-worker-tester-critic` e `fast-worker-reviewer`. Como os modelos mudam, os presets não fixam provider nem modelo: vêm de `[defaults.<role>]` em `loops.toml`, e faltar é erro claro.

## Veredito, ciclos e fim

- A última linha não vazia do artefato do gate deve ser `VERDICT: PASS` ou `VERDICT: RETRY`, aceitando espaços e ênfase Markdown.
- **Veredito ausente ou malformado:** repete só aquele revisor uma vez; se falhar de novo, `ERROR`. Não refaz trabalho bom.
- `max_cycles = 3` permite três passagens pelo gate. `RETRY` na terceira vira `ESCALATE`.
- Etapa que falha não é repetida: `ERROR`, com o motivo.

| Fim | Quando | Saída |
|---|---|---|
| `PASS` | Gate aprovou | 0 |
| `ERROR` | Falha de etapa, sandbox, git ou motor | 1 |
| (uso) | Argumento ou workflow inválido | 2 |
| `ESCALATE` | Ciclos esgotados, travamento, timeout ou limite de duração | 3 |
| `CANCELLED` | `loop stop` ou Ctrl-C | 4 |

O workspace é preservado em todos os casos, até `loop clean`.

## Vigia e aviso único

1. `tchelvis loop run … --detach` valida tudo, registra a run, abre o terminal Herdr, imprime `RUN_ID` e volta na hora. Sem Herdr, `--detach` é erro. Sem `--detach`, o comando espera e imprime as 5 linhas.
2. O motor atualiza `heartbeat_at` a cada 10 s e `last_activity_at` quando a etapa produz saída ou altera o workspace.
3. **Travamento:** sem atividade por `stall_secs`, o motor encerra o grupo de processos da etapa e termina em `ESCALATE` (`STALLED`). O mesmo vale para `stage_timeout_secs`.
4. **Motor morto:** no ciclo de 5 s já existente, o daemon confere pid e horário de início do processo. Se sumiu sem estado final, marca `ERROR` (`ENGINE_DIED`).
5. Com estado final, é gravada uma única mensagem na caixa do `orch` (id fixo por run, o que impede duplicatas), e a entrega já existente acorda o terminal que chamou, só em `idle`/`done` e com a identidade conferida por `created_ms`. O agente roda `tchelvis orch check` e lê: `Loop <run-id>: <STATUS>. Rode: tchelvis loop result <run-id>`.
6. Como em qualquer mensagem do `orch`, o ponteiro pode ser reenviado se o agente não o ler. Se o terminal não existir mais, a run aparece como não entregue em `loop list --runs`.

Dentro do sandbox, encerrar o cliente `incus exec` pode não encerrar o processo. Cada etapa grava seu pid no sandbox e o motor confirma o término dali; se não conseguir, o resultado diz isso claramente.

## Saída de 5 linhas

```text
STATUS: PASS | LOOP: worker-reviewer-critic | CICLOS: 2/3
RUN_ID: 20261123-182500-a1b2
BRANCH: loop/worker-reviewer-critic-20261123-182500-a1b2
ARQUIVOS: src/a.rs, src/b.rs (+12)
RESUMO: <até 140 caracteres>
```

No máximo 10 arquivos; nomes e resumo têm caracteres de controle removidos. O resumo é a última linha antes do veredito, ou o erro. `stdout` nunca passa de 5 linhas, inclusive em erro. O texto vem de agentes: o ponteiro de aviso não o inclui.

## Relatórios

`report.md` (fase A) e `report.html` autocontido (fase B) com o conteúdo da especificação original (§5): resumo, veredito, `--stat`, linha do tempo, consumo e artefatos. O HTML escapa todo conteúdo e leva uma CSP que proíbe scripts e recursos externos. Segredos passam por `redact_secrets`. Falha ao gerar o relatório não muda o `STATUS`.

`state.json` é gravado por arquivo temporário e `rename`, com `schema_version`.

## CLI

| Comando | Fase |
|---|---|
| `loop init`, `new`, `list [--runs]`, `check <workflow>` | A |
| `loop run <workflow> [prompt] [--input-file] [--from-head] [--max-cycles] [--model etapa=modelo] [--detach] [--verbose]` | A |
| `loop status [run-id] [--json]`, `result`, `logs`, `report`, `stop`, `clean` | A |
| `loop resume <run-id>`, `report --html` | B |

`--input-file` e `[prompt]` são exclusivos. `check` valida workflow, providers e sandbox sem gastar tokens. `clean` mostra o que vai apagar, recusa runs ativas e não apaga a branch. Há uma trava por run, e `max_concurrent` vale para todas.

## Fases

| Fase | Entrega |
|---|---|
| **A** | Validação, sandbox `inherited` e `bwrap` (Incus em seguida), git seguro, 3 adapters, motor, vigia e aviso único, 5 linhas, `report.md` |
| **B** | `resume`, `report.html`, `pi` e `codex`, limite de tokens, `checks` determinísticos (ex.: `cargo test`) antes do gate |
| **C** | Aprovação de comandos com `[guardrails]`, `custom`, aba no mod |

## Mudanças em relação à especificação original

| Original | Agora | Por quê |
|---|---|---|
| YAML | TOML | Decisão do usuário |
| Worktree | Clone + branch criada pelo motor | Git no worktree executa código do agente fora do sandbox |
| `supervisor_policy` | Sandbox obrigatório; política na fase C | O supervisor lê telas do Herdr; headless não tem prompt |
| Veredito malformado → `RETRY` | Repete o revisor, depois `ERROR` | Não refazer trabalho bom |
| `on_retry: worker` nos presets | Id da etapa | Contradizia o schema |
| `captures_diff` | Snapshot em toda etapa; `edits = true` | Pega arquivos novos e revisor que edita |
| Linha `WORKTREE` | `BRANCH` | O resultado é uma branch |
| `claude --output-format json` | Fluxo de eventos | Detectar travamento |
| `.gitignore` do projeto alterado | `.gitignore` dentro de `runs/` | Não mexer em arquivo do usuário |
| Só `claude`, `opencode`, `codex`, `pi`, `custom` | Inclui `agy` | É por ele que o usuário usa o Hydrogen |

## Critérios de aceite (fase A)

Com adapters e sandbox falsos: `PASS` no ciclo 1; `RETRY` → `PASS` com somas corretas; `ESCALATE` na terceira reprovação; veredito malformado; revisor que edita; travamento, timeout e cancelamento encerrando o grupo de processos; motor morto detectado; um único aviso, inclusive após reiniciar o daemon; `stdout` com no máximo 5 linhas em todos os fins; workflow inválido em cada regra.

Com git real: hook, `fsmonitor`, `diff.external` e link simbólico plantados não executam nem vazam; arquivos novos e commits do agente entram no diff; checkout do usuário intacto; branch existente recusada.

No ai-box: um loop real em `inherited` e outro em `bwrap`. Depois, no host, com Incus. Em todos, chamado por um Claude que encerra o turno e é acordado uma vez.

## Minhas escolhas para conferir

1. Clone com branch no fim, em vez de worktree.
2. `auto_approve_in_sandbox` precisa ser ligado por você; sem isso, nenhum loop roda.
3. Travamento encerra o loop e chama você, sem tentar de novo.
4. Presets sem modelo fixo.
5. Repositório com mudanças não commitadas é erro por padrão.

## Deixado para quem implementar

Nomes internos, tabela do banco, textos de ajuda e layout do relatório.
