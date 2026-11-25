# Motor de loops: modo sem sandbox (`kind = "none"`) — design

**Data:** 2026-11-25 · **Status:** aprovado pelo usuário em 2026-11-25 (decisões 1 a 4 do pedido e 5 a 7 sobre as recomendações) · **Tamanho:** pequeno (um tipo de sandbox novo, sem mudança no ciclo do motor)

Estende `docs/superpowers/specs/2026-11-23-loop-engine-design.md` (seção Sandbox, linhas 74–110). Onde esta spec não diz nada, vale aquela e a do adapter `pi` (`2026-11-24-loop-phase-b-pi-adapter-design.md`). Código citado é o da `main` em `cae2e75`.

## Intenção

Pedido do usuário: "adicionar o modo normal ao loop, onde ele não entra dentro do bwrap, apenas roda normalmente; o coordenador continua recebendo apenas as 5 linhas". Os motivos aparecem no que foi decidido: sem montagens, sem login próprio do loop e com os CLIs usando o login normal do `HOME`. É a forma de rodar o loop onde o `bwrap` atrapalha (login do `agy` no chaveiro, `pi` fora da instalação gerenciada, toolchains fora de `home_ro`), com o mesmo contrato para quem chama.

## Objetivos, não objetivos e antiobjetivos

**Objetivos:**
- `kind = "none"` roda as etapas como processos comuns do usuário, com `HOME` e rede reais.
- Duas travas no `loops.toml`.
- "Sem sandbox" visível em `check`, `result`, `status` e `report.md`.
- Clone isolado, branch local, ambiente limpo e as 5 linhas continuam como estão.

**Não objetivos:**
- Mudar `inherited`, `bwrap` ou o padrão quando `kind` não é informado (continua `bwrap`).
- `incus`.
- Proteger o estado do `tchelvis` contra o agente nesse modo.
- Fase C (`[guardrails]`).

**Fracasso mesmo funcionando se:**
- o modo fosse escolhido sem o usuário escrever as duas travas no próprio `loops.toml`;
- um repositório conseguisse ligá-lo;
- algum outro tipo de sandbox, ao falhar, caísse para `none`;
- quem lê as 5 linhas ou o relatório não percebesse que a run foi sem sandbox.

## Decisões do usuário (2026-11-25) — fixas

1. **Novo `kind = "none"`** em `[sandbox]`. Não exige container: o agente roda com o usuário, o `HOME` e a rede reais, sem nenhuma montagem. O `inherited` fica como está. O padrão quando `kind` não é informado continua `bwrap`, e nunca há troca automática de tipo.
2. **Duas travas:** `auto_approve_in_sandbox = true` e um segundo aviso explícito no `loops.toml`, nunca lido do repositório. Sem esse aviso, `kind = "none"` falha na validação. `check` e o relatório destacam "sem sandbox".
3. **Mantém** o clone isolado com a branch `loop/<workflow>-<run-id>` (nunca merge nem push), o **ambiente limpo** (`PATH`, `HOME`, `LANG`, `TERM` e `env_pass`) e as **5 linhas**.
4. **Não mantém** o login próprio do loop: os CLIs usam o login normal do `HOME`. O `pi` roda direto, sem `PI_CODING_AGENT_DIR` temporário e sem montagens.

## Abordagens

| | Como | Troca |
|---|---|---|
| **A. Tipo próprio** `SandboxConfig::Unsandboxed` + `NoSandbox: Sandbox` (escolhida) | Uma struct a mais, ao lado de `InheritedSandbox` (`src/loops/sandbox.rs:324-503`); o motor e os adapters não mudam. | As verificações de "sem sandbox" ficam em pontos conhecidos (`kind() == "none"`). |
| B. `inherited` com uma opção que pula o marcador de container | Menos código. | Mistura dois limites diferentes sob um nome só. O usuário pediu `inherited` intacto, e o relatório não conseguiria distinguir os dois. |
| C. Chamar os CLIs sem passar pelo `Sandbox` | Nenhum. | Duplicaria a limpeza de ambiente e a regra por provider (`wrap_for_provider`, `src/loops/sandbox.rs:506-561`). Descartada. |

## Decisões de projeto

### 1. Configuração (`src/loops/config.rs`)

```toml
auto_approve_in_sandbox = true
allow_unsandboxed = true      # segunda trava: só vale com kind = "none"

[sandbox]
kind = "none"
```

- `SandboxConfig` (`config.rs:16-19`) ganha a variante `Unsandboxed`; a string `kind` é `"none"`. O nome evita confundir com `Option::None`.
- `RawLoopsConfig` (`config.rs:69-83`) e `LoopsConfig` (`:59-67`) ganham `allow_unsandboxed: bool`, com padrão `false`. O campo fica no nível de cima, ao lado de `auto_approve_in_sandbox`, e é lido só de `~/.config/tchelvis/loops.toml`, como todo o arquivo.
- O `match` de `kind` (`config.rs:230-241`) ganha `"none"`. Se `allow_unsandboxed` não for `true`, a carga falha com:
  `sandbox 'none' exige allow_unsandboxed = true em ~/.config/tchelvis/loops.toml: o agente roda sem isolamento, com seu HOME, sua rede e seus terminais`.
  Isso vale para todos os comandos que carregam a configuração, como já acontece com `incus`.
- `allow_unsandboxed = true` com outro `kind` não tem efeito; `check` avisa: `aviso: allow_unsandboxed = true não tem efeito com sandbox '<kind>'`.
- `auto_approve_in_sandbox` continua obrigatório para `run` e `check --live` (`src/loops/cli.rs:2583-2592`, `2717-2721`). Sem ele o modo `none` não roda, como os outros.
- O modelo de `loop init` (`cli.rs:1636-1655`) ganha só um comentário: `# kind = "none" roda sem sandbox; exige também allow_unsandboxed = true`. O padrão gerado continua `bwrap`.
- `FrozenRunConfig.sandbox` (`cli.rs:234`) serializa a variante nova. `frozen.json` antigos continuam válidos, porque a mudança só acrescenta.

### 2. `NoSandbox` (`src/loops/sandbox.rs`)

| Método | Comportamento |
|---|---|
| `kind()` | `"none"` |
| `wrap` | Igual ao `InheritedSandbox::wrap` (`sandbox.rs:489-499`): argv intacto, ambiente limpo por `build_clean_env` (`:189-`), `cwd` no workspace |
| `check` | Sem exigir marcador de container. Roda a sonda de escrita no workspace (`probe_workspace_writable`, `:293-322`). Exposições, nesta ordem: (a) a linha fixa da Decisão 5; (b) as detecções do host que o `inherited` já faz e que ajudam a decidir: `sudo` sem senha, sockets do Docker e do Incus graváveis, `hc` no `PATH` (`:452-484`). Essas detecções viram uma função comum, `host_exposures(root)`, usada pelos dois tipos. A busca de montagens compartilhadas (`detect_shared_writable_mounts`) não roda: no host, tudo é "compartilhado" e a lista seria só ruído. |
| `agy_state_dir()` | `None` (padrão do trait): o login próprio do `agy` é ignorado |

O motor (`src/loops/engine.rs`), a sonda (`src/loops/preflight.rs`) e as checagens grátis (`src/loops/free_check.rs`) já recebem `&dyn Sandbox` e não mudam, exceto pelo que vem de `wrap_for_provider` (Decisão 3).

### 3. Ambiente por provider (`wrap_for_provider`, `sandbox.rs:506-561`)

- **Ambiente limpo, igual ao `inherited`:** `PATH HOME LANG TERM` + `env_pass`.
- **`OPENCODE_API_KEY`:** sem mudança; só chega a etapas, sondas e checagens do `opencode`. Limite: sem sandbox, o agente pode ler os arquivos do shell do usuário, onde a chave costuma estar exportada.
- **`pi`:** sem `PI_CODING_AGENT_DIR`. O ramo atual só age com `kind() == "inherited"` (`:523`), então nada muda no código. O argv do adapter continua o mesmo em todos os modos (`--no-extensions`, `--no-skills`, `--no-mcp` etc.); o adapter não conhece o sandbox. Consequência: o `pi` lê o `~/.pi/agent` real, ou seja, `settings.json`, o `AGENTS.md` global e o catálogo atualizável; extensões, skills e MCP continuam desligados pelas opções, e nada vai para `sessions/` (`--no-session`).
- **`pi` fora da instalação gerenciada:** só é exigida no `bwrap`, que precisa das montagens. `pi_check_notes` (`src/loops/cli.rs:958-971`) passa a receber o tipo de sandbox e não falha com `none` nem `inherited`. O aviso sobre `auth.json` continua.
- **`agy` e o chaveiro** (Decisão do usuário 5): o login normal do `agy` fica no chaveiro, alcançado por D-Bus (spec do motor, linhas 108–110). O ambiente limpo não tem D-Bus, então o "login normal do `HOME`" não funcionaria. No modo `none`, só as etapas, sondas e checagens do `agy` recebem também `DBUS_SESSION_BUS_ADDRESS` e `XDG_RUNTIME_DIR`. Vêm do ambiente do `tchelvis`; se faltarem, são derivadas do uid: `/run/user/<uid>` e `unix:path=/run/user/<uid>/bus`, só se o socket existir. A exceção segue a do `OPENCODE_API_KEY`. Ela não aumenta a exposição: sem sandbox, o agente já alcança o socket pelo sistema de arquivos. O `NoSandbox` calcula esses pares uma vez, no construtor (`agy_session_env`), e `wrap_for_provider` os aplica com `apply_extra_env` depois do `wrap`, só quando `provider == Agy` **e** `kind() == "none"`. Assim valem para etapas, sondas e checagens grátis, que passam todas por `wrap_for_provider`. Nenhum outro provider os recebe, e o `agy` não os recebe em `bwrap` nem em `inherited`. O `Sandbox` ganha `fn agy_session_env(&self) -> &[(String, String)]`, com padrão vazio.

### 4. Contas, `loop auth` e evidência

| Peça | No modo `none` |
|---|---|
| `loop auth` (`cli.rs:2055-2176`) | Primeira linha: `modo sem sandbox: os CLIs usam os logins normais do HOME`. `claude`, `opencode` e `pi` mostram o mesmo de hoje; as checagens grátis rodam pelo `NoSandbox`. Para o `agy`, a conta está ok se houver o arquivo padrão **ou** se `agy models` listar modelos. Se não estiver: `conta do 'agy' não encontrada; sem sandbox o agy usa o login normal (rode 'agy' e entre na conta)`. Se existir `<state>/loops/agy-login`, uma linha diz que ele não é usado nesse modo. |
| `loop auth login <cli>` (`cli.rs:2178-`) | `claude`, `opencode` e `pi <provedor>` sem mudança (já são os logins oficiais). `agy` abre o `agy` puro, sem `bwrap` e sem `AGY_CLI_FORCE_FILE_TOKEN_STORAGE`. |
| Conta ok nas checagens grátis (`cli.rs:1185-1200`, `free_check.rs:430-440`) | `agy`: arquivo presente **ou** lista conhecida. Os demais, sem mudança. |
| Identidade de conta (`cli.rs:1034-1115`) | As mesmas funções. `CliIdentity.sandbox = "none"` já separa essa evidência da do `bwrap` (`evidence.rs:23-27`), então nada se mistura. `agy` usa `AgyDefaultFile(presente)`, o mesmo limite já aceito para o arquivo padrão. |

### 5. O que o usuário e o coordenador veem

- **`loop check`** (`cli.rs:2538-2549`): a primeira linha vira `Workflow '<wf>' válido (sandbox: none — SEM SANDBOX).` A primeira exposição é a linha fixa:
  `SEM SANDBOX: o agente roda como o seu usuário, com HOME, rede, repositório principal, estado do tchelvis (~/.config, ~/.local/share e ~/.local/state/tchelvis) e terminais Herdr ao alcance`.
  Seguem as detecções do host (Decisão 2).
- **5 linhas** (`format_result`, `src/loops/state.rs:316-356`; `cmd_status`, `cli.rs:3339-3341`): quando `state.sandbox == "none"`, a linha 1 termina em ` | SEM SANDBOX`. Exemplo: `STATUS: PASS | LOOP: worker-reviewer-critic | CICLOS: 2/3 | SEM SANDBOX`. Continuam 5 linhas, e o prefixo `STATUS:` não muda. A saída sem estado (`cli.rs:3409-3412`) não tem o tipo e fica como está. `--detach` imprime o mesmo de hoje.
- **`report.md`** (`src/loops/report.rs:198`): `- **Sandbox:** **SEM SANDBOX** (\`none\`): o agente rodou com o HOME, a rede e os poderes do seu usuário`. A linha de exposições continua.
- **`state.json`, `loop list --runs`, `--verbose`**: já registram `sandbox.kind()` (`cli.rs:3022`, `:3225`).
- **Coordenador de tickets** (Decisão do usuário 6): na skill `/orchestration-tchelvis` (texto em `src/orch/preamble.rs`, seção `## Running Tickets with Loops`), um `PASS` cuja linha 1 termina em `| SEM SANDBOX` não é juntado sozinho: o coordenador mostra ticket, `RUN_ID` e `BRANCH` e pergunta ao usuário. Com o sim, seguem as regras de sempre (nomes dentro de `files`, `.gitattributes`/`.gitmodules`); sem ele, `status: failed`. Motivo: sem sandbox, o agente pode reescrever o `state.json` e, com ele, o `PASS` e os `ARQUIVOS`. A spec do coordenador (`2026-11-24-tickets-coordinator-design.md`, seção 2, "Ao acordar") ganha a mesma regra.
- **Vigia e aviso único:** sem mudança. O ponteiro do `orch` (`Loop <id>: <STATUS>. Rode: ...`) continua sem o tipo; quem acorda lê as 5 linhas.

### 6. Limites aceitos (como no `inherited`, e mais)

- O agente alcança **o estado do `tchelvis`**: `loops.toml`, `state.json`, a evidência e `orch.db`. Pode reescrever a configuração das próximas runs (inclusive as duas travas), o próprio `state.json` (e, com ele, as 5 linhas e o `PASS`) e mensagens do `orch`. `TCHELVIS_HOME_DIRS` (`config.rs:9-13`) protege só no `bwrap`.
- Alcança o **repositório principal**, outros projetos, `hc` (outros terminais), `sudo` sem senha, Docker e Incus, se existirem. `check` lista os que encontrar.
- O git seguro do motor continua protegendo o motor (hooks, `fsmonitor`, `diff.external` não rodam fora do agente), mas não é fronteira contra o agente.
- Renovação de token agora é gravada (o `HOME` é real). É um efeito bom, mas uma etapa também pode sair da conta ou trocá-la.

## Testes (sem rede, sem `$HOME` real, sem `bwrap`)

- **Configuração:**
  - `none` com as duas travas carrega `Unsandboxed`;
  - `none` sem `allow_unsandboxed`, ou com `false`, falha com a mensagem exata;
  - `allow_unsandboxed = true` com `bwrap` carrega e não tem efeito;
  - `kind` ausente continua `bwrap`;
  - `frozen.json` com `Unsandboxed` faz ida e volta.
- **`NoSandbox`:**
  - `check` com raiz falsa e sem marcador de container passa;
  - a primeira exposição é a linha fixa;
  - `sudo`, `hc` e sockets falsos aparecem;
  - nenhuma linha de "montagem gravável";
  - workspace não gravável falha;
  - `wrap` com o ambiente de processo falso: só as variáveis permitidas e as de `env_pass`.
- **`wrap_for_provider` com `NoSandbox`:**
  - `pi` sem `PI_CODING_AGENT_DIR`;
  - `opencode` com a chave, os outros sem ela;
  - `agy` com os dois pares de D-Bus; `claude`, `opencode` e `pi`, com o mesmo `NoSandbox`, sem nenhum deles;
  - `agy` em `InheritedSandbox` (e `bwrap`) sem D-Bus, mesmo com as variáveis no ambiente do processo;
  - sem socket nem variáveis, `agy` não recebe nada.
- **Saída:**
  - `format_result` com `sandbox = "none"`: 5 linhas, linha 1 terminando em ` | SEM SANDBOX`; com `bwrap`, idêntica à de hoje;
  - `report.md` com o destaque;
  - linha do `check` (helper puro).
- **Contas:**
  - `agy` com conta ok pela lista, sem arquivo;
  - argv do login do `agy` no modo `none` = `["agy"]`;
  - `pi_check_notes` com `none` e instalação não reconhecida dá `Ok` com o aviso.
- **Skill do coordenador:** `orchestration_skill_documents_running_tickets` (`src/orch/preamble.rs:279-`) passa a exigir a frase da regra `SEM SANDBOX`, e a regra vem antes da de juntar a branch.
- **Motor:** uma run de ponta a ponta com `NoSandbox` e CLIs falsos (padrão dos testes de `ProcessStageRunner` em `src/loops/engine.rs:2340-2480`) termina em `PASS`, com `state.sandbox == "none"` e branch criada.

Validação real, fora do CI: no ai-box, uma run `none` com `claude`, outra com `agy` usando o login no chaveiro e outra com `pi` fora da instalação gerenciada, se houver uma. Também `loop check` mostrando o destaque e o coordenador recebendo as 5 linhas com ` | SEM SANDBOX`.

## Documentação

- **Spec do motor:** a tabela da seção Sandbox ganha a linha `none` e uma nota que aponta para esta spec. A linha de configuração `kind = "bwrap"  # padrão; bwrap | inherited | incus` (linha 84) passa a incluir `none`.
- **README, `docs/TCHELVIS-ORCH.md`, CHANGELOG e ROADMAP:** `kind = "none"`, as duas travas e os limites.

## Decisões do usuário (2026-11-25), sobre as recomendações

5. **D-Bus só para o `agy`, só no modo `none`:** `DBUS_SESSION_BUS_ADDRESS` e `XDG_RUNTIME_DIR` entram no ambiente das etapas, sondas e checagens do `agy` (Decisão de projeto 3); nenhum outro provider os recebe.
6. **O coordenador pergunta antes de juntar** a branch de uma run `SEM SANDBOX` (Decisão de projeto 5).
7. **Nome da trava:** `allow_unsandboxed = true`, no nível de cima do `loops.toml`.

## Deixado para quem implementar

Nomes internos (`NoSandbox`, `host_exposures`). Plano: `docs/superpowers/plans/2026-11-25-loop-sandbox-none.md`.
