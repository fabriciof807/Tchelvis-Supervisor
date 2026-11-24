# Motor de loops, fase B: adapter `pi` e ordem da fase — design

**Data:** 2026-11-24 · **Status:** aprovado pelo usuário em 2026-11-24 (as 6 recomendações; ver "Decisões do usuário") · **Tamanho:** pequeno no `pi` (um adapter no padrão dos três existentes); a fase B inteira é um projeto, dividido em specs abaixo.

Estende `docs/superpowers/specs/2026-11-23-loop-engine-design.md` (seção Adapters, linhas 116–126; Fases, linhas 255–261). Onde não diz nada, vale aquela spec.

Convenção de evidência: **[cap]** = captura de 2026-11-24 em `.work/loop-captures/` (git-ignorado), **[help]** = `pi --help`/`pi auth --help` 1.5.2, **[doc]** = documentação instalada com o `pi` (`~/.pi/agent/install/releases/1.5.2/node_modules/@earendil-works/pi-coding-agent/docs/`), **[código]** = código do `pi` instalado, **[suposição]** = não verificado.

## Intenção

O usuário tentou usar `pi` como provider de uma etapa e só `claude`, `agy` e `opencode` funcionam: `Provider` não tem `pi` (`src/loops/workflow.rs:77-112`) e `command()` só monta esses três (`src/loops/adapters.rs:22-103`). O `pi` é o CLI pelo qual o usuário acessa vários provedores (hoje `opencode-go`, com chave em `~/.pi/agent/auth.json`), então entrar no loop significa poder escalar modelos baratos em etapas de worker e de revisão, como o coordenador de tickets pede (`docs/superpowers/specs/2026-11-24-tickets-coordinator-design.md:87-91`).

## Objetivos, não objetivos e antiobjetivos

**Objetivos:** `provider = "pi"` em workflow, `models.toml` e `--use etapa=pi:<provedor>/<modelo>[:<thinking>]`; telemetria (turnos, tokens, custo) no padrão dos três; erro de modelo inválido detectado mesmo com saída 0; `loop check` (grátis e `--live`) e `loop auth` cobrindo o `pi`; isolamento equivalente ao do `claude`.

**Não objetivos:** `codex`, `custom`, `[guardrails]`, `checks` (só posicionados, abaixo); modo `rpc` do `pi`; sessões persistentes ou `resume`; suportar instalações do `pi` que não sejam a gerenciada (Decisão do usuário 4).

**Fracasso mesmo funcionando se:** uma etapa `pi` carregasse extensões, skills, MCP, `AGENTS.md` global ou o histórico de sessões do usuário; gravasse em `~/.pi`; ou um modelo inexistente desse `PASS` porque o processo saiu com 0.

## Abordagens consideradas

| | Como isola a configuração do usuário | Troca |
|---|---|---|
| **A. Só opções de linha de comando** sobre o `~/.pi/agent` real | `--no-extensions --no-skills --no-mcp --no-context-files ...` | Simples, mas o `settings.json` (modelo e provedor padrão, pacotes) ainda vale; o `AGENTS.md` global só sai com `--no-context-files`, que tira também o do projeto; no `bwrap` exigiria montar `~/.pi/agent` inteiro, com `sessions/` (histórico com possíveis segredos) visível. Contraria "cada etapa vê só o executável e a credencial" (spec do motor, linhas 100–110). |
| **B. Diretório de agente mínimo + opções** (escolhida) | No `bwrap`/`incus`, o `$HOME` já é `tmpfs` (`src/loops/sandbox.rs:582-605`): montar só executável, `auth.json` e `models-store.json` faz o `~/.pi/agent` visto pela etapa conter apenas isso. No `inherited`, `PI_CODING_AGENT_DIR` aponta para um diretório temporário com links para os mesmos arquivos. As opções de linha de comando valem nos dois. | Dois caminhos de montagem, mas cada um curto; nenhuma configuração global entra, e o `AGENTS.md` do projeto pode continuar (paridade com `--setting-sources project` do `claude`). |
| **C. `--mode rpc`** com o motor conversando com o `pi` | Igual a B | Protocolo bidirecional, longo; nada que o motor precise (cada etapa é uma sessão nova). Descartada. |

Escolhida: **B**. `PI_CODING_AGENT_DIR` é documentado [help] e funcionou na captura com um diretório que só tinha links para `auth.json` e `models-store.json` [cap].

## Decisões

### 1. Comando

```
pi -p --mode json --no-session --offline --no-approve
   --no-extensions --no-skills --no-mcp --no-prompt-templates --no-themes
   --model <provedor>/<id>[:<thinking>]
```

Prompt por **stdin**, como `claude` e `opencode` (`src/loops/adapters.rs:46-49`, `97-100`).

| Opção | Por quê | Fonte |
|---|---|---|
| `-p --mode json` | Fluxo JSONL, um objeto por linha, stdout reservado ao fluxo, diagnósticos no stderr | [help], [doc] `json.md` |
| `--no-session` | Cada etapa é sessão nova e nada grava em `sessions/` | [help] |
| `--offline` | Sem atualização de catálogo nem rede de inicialização; o `models-store.json` fica somente leitura | [help], [doc] `environment-variables.md`; a etapa funcionou com ele [cap] |
| `--no-approve` | Ignora `.pi/` do workspace (extensões, `settings.json`, `mcp.json`, `SYSTEM.md`): uma etapa anterior poderia plantá-los para a seguinte | [help], [doc] `security.md` |
| `--no-extensions --no-skills --no-mcp --no-prompt-templates --no-themes` | Defesa em profundidade além do diretório mínimo; desliga também extensões embutidas | [help] |
| `--model` | Sempre presente, `provedor/id`, com sufixo `:<thinking>` opcional; ver Decisão 5 | [help] |

- **Sem `--no-context-files`** (Decisão do usuário 2): o `AGENTS.md`/`CLAUDE.md` do projeto continua, como no `claude`; o global não existe no diretório mínimo.
- **Sem `--tools`**: o padrão (`read`, `bash`, `edit`, `write`) é o necessário [help]; `edits = false` continua garantido pelo snapshot do motor, não pelo CLI.
- **Sem `--provider`**: o prefixo `provedor/` em `--model` já escolhe o provedor [help]; `--provider` só faz sentido com padrão aproximado de modelo, que não aceitamos.
- **Tamanho do prompt:** stdin é lido inteiro (`readPipedStdin`, `dist/main.js:52-70` [código]); não há limite como o de 100 KiB do `agy` (`src/loops/adapters.rs:8`). Sem limite novo.

### 2. Fluxo de eventos e telemetria

Formato [doc] `json.md`, confirmado [cap] `pi_ok.jsonl`: `session` → `agent_start` → `turn_start` → `message_start/update/end` (system, user, assistant) → `turn_end` → `agent_end` → `agent_settled`. Com erro e nova tentativa automática aparecem `auto_retry_start` / `auto_retry_end` [cap] `pi_bad_model.jsonl`.

| O motor precisa de | Vem de |
|---|---|
| Turnos | Contagem de `turn_end` com `message.role == "assistant"` e `stopReason` fora de `error`/`aborted`; zero vira `null` |
| Tokens de entrada, saída, cache lido | Soma de `message.usage.input`, `.output`, `.cacheRead` em cada `message_end` do assistente (o `usage` é por resposta [doc]); como no `opencode` (`src/loops/adapters.rs:316-328`), um campo ausente em alguma resposta deixa a soma `null` |
| Custo | Soma de `message.usage.cost.total`, **inclusive com `totalFromProvider: false`** [cap] (estimativa do catálogo do `pi`, como a do `claude`; Decisão do usuário 3). Ausente em alguma resposta → `null` |
| Atividade (trabalho × travamento) | Toda linha já pulsa o vigia (`src/loops/engine.rs:155-160`); o `pi` emite `message_update` por fragmento e `tool_execution_*` por ferramenta [doc]. Nada novo. |
| Texto final | Não é usado: `AdapterResult` só tem `usage` e `error` (`src/loops/adapters.rs:16-20`); o veredito vem do arquivo. |

**Erro.** No modo `json`, o `pi` **sai com 0 mesmo quando o modelo falha**: só o modo texto converte `stopReason == "error"` em código 1 (`dist/modes/print-mode.js`, bloco `if (mode === "text")` [código]); a captura com modelo inexistente saiu com 0 [cap]. O parser decide:

1. `auto_retry_end` com `success: false` → erro = `finalError`.
2. Senão, a última mensagem do assistente (`message_end`) com `stopReason` `error` ou `aborted` → erro = `errorMessage` (ou "pi retornou stopReason <x>").
3. `agent_settled` com `aborted: true` → erro "pi abortou a execução".
4. `stopReason == "length"` na última mensagem → erro "resposta truncada no limite de saída" (o modo texto do `pi` faz o mesmo [código]).
5. Fim sem `agent_settled` e sem erro → "pi terminou sem evento 'agent_settled'" (equivalente ao "sem evento 'result'" dos outros, `src/loops/adapters.rs:303-315`).

Texto passa por `sanitize_error_text` (`src/loops/adapters.rs:105-113`). O motor já trata saída 0 com erro do parser como falha da etapa (`src/loops/engine.rs:181-195`) e da sonda (`src/loops/preflight.rs:242-257`). Formato inesperado só deixa telemetria `null`, sem falhar (regra da spec do motor, linha 124), exceto a regra 5, que é a mesma dos outros adapters.

O `pi` repete a chamada até 3 vezes com espera de 2, 4 e 8 s antes de desistir [cap]: cerca de 15 s extras numa sonda com modelo inválido, dentro dos 60 s de `PREFLIGHT_TIMEOUT_SECS` (`src/loops/preflight.rs:51`).

### 3. Aprovação automática

Confirmado: o `pi` não tem barreira de permissão; "não pede aprovação antes de cada chamada de ferramenta" [doc] `security.md`, e não há opção de aprovação no `--help` (`--approve` é confiança em arquivos do projeto, não em ferramentas). O adapter não acrescenta nada. `auto_approve_in_sandbox` continua sendo a trava global para qualquer loop rodar (`src/loops/cli.rs:2418-2425`, `2552-2554`); não muda o comando do `pi`. A frase da spec do motor "as opções de aprovação automática de cada CLI só são usadas com…" (linha 121) vale trivialmente.

### 4. Sandbox, credenciais e montagens

`bwrap`/`incus` — o que o `pi` precisa do `$HOME`, sempre somente leitura (hoje `ADAPTER_CREDENTIALS` só conhece um arquivo por CLI, `src/loops/sandbox.rs:31-36`, e `adapter_binds` monta o diretório do executável real, `498-546`):

| Caminho | Por quê |
|---|---|
| `~/.local/bin/pi` → `~/.pi/agent/bin/pi` | Lançador: script `sh` que lê `install/current-version` e executa a release [cap] |
| `~/.pi/agent/install/` | Releases e `current-version` |
| `~/.local/share/pi-node/` (ou `$XDG_DATA_HOME/pi-node`) | Node instalado pelo `pi`; `current` é link absoluto para dentro dele |
| `~/.pi/agent/auth.json` | Credencial (chave ou OAuth). Conteúdo nunca lido pelo `tchelvis` |
| `~/.pi/agent/models-store.json` | Catálogo baixado; sem ele o `pi` não conhece modelos recentes (com só `auth.json`, `hy4-flash-free` não aparece) [cap] |
| `~/.pi/agent/models.json`, se existir | Provedores e modelos próprios do usuário [doc] `configuration.md` |

Nada mais de `~/.pi/agent` entra: nem `settings.json`, `AGENTS.md`, `extensions/` (inclusive a `tchelvis.js` instalada pelo `setup`), `sessions/`, `trust.json`. Escritas do `pi` em `~/.pi/agent` caem no `tmpfs` e somem.

`inherited`: `PI_CODING_AGENT_DIR=<diretório temporário da etapa>` com links para `auth.json`, `models-store.json` e `models.json`. É passado pelo ambiente (`wrap_with_extra_env`, `src/loops/sandbox.rs:57-66`), não por argumento. O lançador e o Node vêm do `PATH` normal.

Ambiente: base `PATH HOME LANG TERM` + `env_pass` (`src/loops/sandbox.rs:14`, `178-210`). Chaves por variável (`ANTHROPIC_API_KEY` etc. [help]) só entram se o usuário as puser em `env_pass`. **`OPENCODE_API_KEY` não é repassada ao `pi`** (continua só para etapas `opencode`, `src/loops/engine.rs:121-136`): para o `pi`, a fonte é o `auth.json`.

Limite comum (spec do motor): o agente lê a própria credencial — aqui, **todas** as chaves do `auth.json`, não só a do provedor da etapa. Aceito (Decisão do usuário 5); `loop check` com etapa `pi` imprime o aviso `pi: a etapa vê todas as credenciais de ~/.pi/agent/auth.json`.

**Instalação reconhecida** (Decisão do usuário 4): só a gerenciada — `pi` no `PATH` resolve para `~/.pi/agent/bin/pi` e `~/.pi/agent/install/current-version` é um arquivo regular. Caso contrário, `loop check` e `run` falham com `instalação do pi não reconhecida (só a instalação gerenciada em ~/.pi/agent é suportada)`.

### 5. Modelos expostos

- Forma aceita em workflow, `models.toml` e `--use`: `<provedor>/<id>` com `:<off|minimal|low|medium|high|xhigh|max>` opcional, ex. `pi:opencode-go/glm-5.3:high`. `parse_use_flag` corta no primeiro `:` (`src/loops/models.rs:186-192`), então o sufixo fica no modelo sem mudança. Validação própria do `pi` ao lado de `validate_model_name` (`src/loops/workflow.rs:196-208`): exige `provedor/id` com as duas partes não vazias e sem espaço; o sufixo após o último `:` só é lido como thinking quando é um dos sete níveis, senão faz parte do id (ids como `llama3:8b` existem) [suposição sobre o `pi`; conferir na validação no host]. Padrões aproximados (`sonnet`, `*haiku*`), aceitos pelo `pi` [help], são recusados: a evidência e a lista precisam de um nome exato.
- **Modelo obrigatório para `pi`** (Decisão do usuário 1): sem `settings.json` não há modelo padrão do usuário. Verificado depois da resolução (`resolve_workflow`, `src/loops/models.rs:307-422`), que é por onde passam workflow, `models.toml`, `--use` e padrões; mensagem: `etapa '<id>': 'pi' exige modelo no formato provedor/id`.
- Thinking: sem sufixo, o `pi` usou `high` com o `hy4-flash-free` [cap] (`thinkingLevel` no `message_end`). Fica como está; não há campo novo no workflow.
- Lista para `loop check`: `pi --offline --list-models`, tabela com cabeçalho `provider model context max-out thinking images`; cada linha vira `provedor/id` [cap] `free_pi_models.txt`. A lista mostra só provedores com credencial [cap: só `opencode-go` aparece; `anthropic` está `not_ready`]. Busca sem resultado imprime `No models matching "..."` e sai com 0 [cap] — o parser deve tratar a saída, não o código. Comparação ignora o sufixo `:<thinking>`.

### 6. Preflight, conta e evidência

| Peça | Como |
|---|---|
| Lista grátis | `pi --offline --list-models` dentro do sandbox (`run_one_free_check`, `src/loops/free_check.rs:276-280`); `parse_model_list` e `is_model_listed` ganham o ramo `Pi` (`115-179`) |
| Conta | `pi auth check --provider <p> --json --no-refresh` por provedor distinto do workflow: `{"status":"ready"}` saída 0, `not_ready` saída 1 (`credentials_not_configured`, `provider_not_found`), `invalid` saída 2 [cap] `free_pi_auth_check.txt`. Nunca com `--credentials`. `auth check --model` **não** valida o id (deu `ready` para modelo inexistente [cap]), por isso a lista continua necessária |
| Sonda `--live` | O próprio comando da Decisão 1 com `PREFLIGHT_PROMPT`; como o `claude` (`src/loops/preflight.rs:202-207`), acrescenta `--no-tools` e `--system-prompt "Reply briefly."` [help] para gastar pouco |
| Identidade da conta | Metadados de `~/.pi/agent/auth.json` (novo `AccountIdentityInput::PiAuthFile`, como `ClaudeCredentialFile`, `src/loops/evidence.rs:30-46`, `566-580`), sem ler o conteúdo; renovação OAuth regrava o arquivo e custa uma sonda a mais |
| Identidade do executável | `compute_executable_identity` resolve o lançador, que não muda quando o `pi` atualiza (`src/loops/evidence.rs:538-564`). Para o `pi`, entra também o conteúdo de `install/current-version` |
| `loop auth` | Linha `pi: <n> modelo(s) de <provedores> (pi --list-models)` a partir da mesma lista grátis (ela só mostra provedores com credencial [cap]); lista vazia ou ilegível: `pi: nenhum provedor com credencial (rode 'tchelvis loop auth login pi <provedor>')`; instalação não reconhecida: a mensagem da Decisão 4; `loop auth login pi <provedor>` abre `pi auth login <provedor>` num terminal Herdr (`src/loops/cli.rs:2013-2056`). Sem o provedor: erro de uso `informe o provedor: tchelvis loop auth login pi <provedor>` (Decisão do usuário 6) |
| Modo `free` | Igual aos outros: modelo listado e conta pronta bastam (`src/loops/free_check.rs:181-238`) |

O aviso do stderr `Model "x" not found for provider "y". Using custom model id.` [cap] `pi_bad_model.stderr.txt` não é lido: o stderr vai só ao log (`src/loops/process.rs:910-934`). Não precisa: a lista grátis e o erro no fluxo já cobrem.

### 7. Coordenador de tickets

Nada muda na skill; o limite "O loop só usa…" da spec do coordenador (linha 130) foi atualizado para incluir `pi`. Um time com etapa `pi` passa a ser oferecido quando `loop check` passar com as Decisões 5 e 6 (spec do coordenador, linhas 87-91).

### 8. Pontos de código tocados (para o plano)

`src/loops/workflow.rs:77-112` (`Provider::Pi`, `"pi"`), `196-208`; `src/loops/adapters.rs` (`command`, `EventParser` com estado do `pi`, testes); `src/loops/sandbox.rs:31-36`, `498-546` (montagens por adapter: lista de caminhos em vez de um arquivo); `src/loops/engine.rs:121-136` e `src/loops/preflight.rs:210-219` (ambiente por provider: generalizar o `match` do `opencode`); `src/loops/free_check.rs` (argv, parser, conta); `src/loops/cli.rs:919-990` (identidade), `1015-1105` (checks grátis), `1929`, `2013-2056` (auth); `src/loops/evidence.rs` (`PiAuthFile`); mensagens "use claude, agy ou opencode".

## Testes

Sem rede, no padrão de `src/loops/adapters.rs:384-` e `src/loops/free_check.rs:590-`:

- Fixtures em `tests/fixtures/loop/`: `pi_ok.jsonl` e `pi_bad_model.jsonl` (das capturas, já anonimizadas; podem ser encurtadas mantendo `session`, um `message_update`, `message_end`, `turn_end`, `auto_retry_*`, `agent_settled`), `free_pi_models.txt`, `free_pi_no_match.txt`, `free_pi_auth_check.txt`.
- `argv_is_exact` com e sem modelo (`src/loops/adapters.rs:409-`).
- `ok_fixtures`: turnos 1, entrada 1493, saída 39, cache 0, custo 0.0, sem erro.
- `bad_model_fixtures_report_an_error`: erro contém "isn't included in OpenCode Go"; turnos `null`.
- Casos sintéticos: `stopReason` `aborted` e `length`; `agent_settled.aborted = true`; sem `agent_settled`; `usage` ausente em uma de duas respostas (soma `null`); lixo e campos renomeados → telemetria `null` sem falhar (`src/loops/adapters.rs:573-`); erro com segredo é redigido.
- Lista: tabela válida, `No models matching`, saída de erro → lista vazia; sufixo `:high` ignorado na comparação; nome sem `/` recusado na validação.
- Sandbox: `bwrap` gera as montagens da Decisão 4 a partir de um `$HOME` falso, e nunca `settings.json`, `sessions/` ou `extensions/`; `inherited` define `PI_CODING_AGENT_DIR` só pelo ambiente; `OPENCODE_API_KEY` ausente no ambiente da etapa `pi`.
- Engine/preflight com executável `pi` falso (como os existentes em `src/loops/preflight.rs:591`, `866`): saída 0 com `pi_bad_model.jsonl` falha a etapa e a sonda.

Validação real (fora do CI, como foi feita para os três): no `bwrap` do ai-box, uma etapa de sucesso e o teste exigido pela spec do motor (linha 104): pedir escrita fora do workspace e confirmar que não acontece. Depois, no host com `incus`.

## Riscos e dependências do host

1. **Não testado dentro do `bwrap`** (só duas execuções reais permitidas nesta rodada, feitas fora dele). Em especial: o diretório `install/` teve o `mtime` alterado durante as execuções [cap: `ls -la` antes/depois], o que sugere arquivo temporário ou trava criada ali; montado somente leitura isso pode falhar. **Validação obrigatória no host antes de considerar a entrega pronta:** uma etapa `pi` real no `bwrap` com `install/` somente leitura, e o pedido de escrita fora do workspace. **Plano B**, se a etapa falhar por escrita em `install/`: montar somente leitura só `install/current-version` e `install/releases/<conteúdo de current-version>/`, deixando o resto de `install/` no `tmpfs` (gravável e descartado).
2. **OAuth** (`anthropic`, `openai-codex` via `pi auth login`): a renovação dentro do sandbox não grava no `auth.json` somente leitura; só foi testada chave de API. Mesmo limite do `claude` e do `agy` na spec do motor.
3. **Chave por comando**: o `auth.json` aceita `"!comando"` para ler de gerenciador de segredos [doc] `providers.md`; o comando rodaria dentro do sandbox, sem D-Bus nem sockets, e provavelmente falha. `loop check` mostra o erro do `auth check`.
4. **Instalação não gerenciada** (npm global, Nix): montagens diferentes; hoje só a gerenciada foi vista no host (`~/.pi/agent/install/managed-install.json`).
5. **Versão**: o formato é de `pi 1.5.2`; um formato novo só deixa telemetria `null`, mas a regra 5 da Decisão 2 falharia a etapa se `agent_settled` sumir.
6. **Custo zero enganoso**: com `totalFromProvider: false` o custo é calculado pelo catálogo do `pi`; modelo sem preço no catálogo daria 0 [suposição].

## Decisões do usuário (2026-11-24)

Todas conforme a recomendação:

1. **Modelo obrigatório** no `pi`, no formato `provedor/id[:thinking]`; sem ele, erro de validação.
2. **`AGENTS.md`/`CLAUDE.md` do projeto** entram nas etapas `pi` (sem `--no-context-files`), como no `claude`.
3. **Custo estimado é somado** mesmo com `totalFromProvider: false`.
4. **Só a instalação gerenciada** na primeira entrega; `loop check` diz quando não a reconhece.
5. **Todas as chaves do `auth.json` visíveis** à etapa são aceitas; `loop check` registra o aviso.
6. **`loop auth login pi <provedor>`** exige o provedor.

## Fase B: ordem e divisão

| # | Item | Spec | Depende de | Escopo em poucas linhas |
|---|---|---|---|---|
| 1 | Adapter `pi` | **esta** | — | Acima. |
| 2 | `checks` determinísticos antes do gate | própria | — | Lista de comandos por workflow (ex. `cargo test`) rodados no sandbox depois do worker e antes do revisor/gate; falha vira `RETRY` com a saída como entrada. Desbloqueia o "PASS garantido" que o coordenador de tickets ainda não tem (linha 129 da spec dele). Maior valor para o usuário depois do `pi`. |
| 3 | Modelo reserva por etapa | própria | preflight/evidência atuais; 1 ajuda (mais modelos) | `fallback = "cli:modelo"` por etapa, testado no preflight como os demais; usado só se o primeiro falhar por erro de modelo/conta no meio do loop, nunca por `RETRY`. Precisa definir o que conta como "falhou". |
| 4 | Limite de tokens | própria, curta | telemetria dos adapters (1 deixa o `pi` completo) | `max_tokens` por run; com somas parciais (`≥`), decidir se o parcial conta. Encerra como `MAX_DURATION` faz hoje. |
| 5 | `loop resume <run-id>` | própria | estado e workspace atuais; interage com 3 | Retomar run parada (travamento, timeout, motor morto) a partir da última etapa concluída, revalidando sandbox e modelos; maior mudança no motor, por isso depois dos itens que não mexem no ciclo. |
| 6 | `report --html` | própria, curta | `report.md` | Mesma informação do `report.md` em HTML estático; nenhum dado novo. Menor urgência. |
| 7 | Adapter `codex` | própria, curta, reaproveita esta | `codex` instalado no host | Capturas (sucesso, modelo inválido), argv, parser, montagens; bloqueado enquanto o CLI não estiver instalado. |

## Deixado para quem implementar

Nomes internos, forma da tabela de montagens por adapter e se as fixtures do `pi` são encurtadas. Plano: `docs/superpowers/plans/2026-11-24-loop-pi-adapter.md`.
