# Handoff — Cockpit (Unified Fleet Control Plane + Durable Supervisor)

**Data da última atualização:** 2026-11-16  
**Repositório:** `https://github.com/fabriciof807/Tchelvis-Supervisor` (público, branch `main`; antigo `cockpit` redireciona)  
**Binário:** `tchelvis` (antes `cockpit`). Instalação/atualização via release: `install.sh` (1ª vez) e `tchelvis update` (canais `stable`/`nightly`). Primeiro stable publicado: **`v0.2.0`** (`a6a81d1`, 2026-11-16); nightly atual = `a6a81d1`.

---

## 1. Visão Geral do Estado Atual

O **Cockpit** unifica o **Fleet Attention Queue & 4-Tier Context Correlator**, o **Cockpit Home & Tarefas do Dia** e o **Pi-Durable FSM Supervisor** (`lfm3:1.7b` via Ollama local com `num_gpu: 0` / Zero-VRAM) em um único binário Rust (`cockpit`), TUI (`cockpit tui`) e Herdr Desktop Mod (`assets/herdr-app/mods/cockpit/main.js` + `assets/herdr-app/layouts/cockpit.json`).

### Principais Módulos do Repositório
- **`src/herdr.rs` & `src/correlate.rs` & `src/daemon.rs`**:
  - Integração JSON-RPC (`hc connect` / `terminal.list`) e stream de eventos (`hc events --json`) com histerese assimétrica de `1.5s` para `blocked` e rollup de subagentes (`blocked_children`).
  - **Filtro inteligente de shells ociosos (`is_plain_shell_foreground`)**: detecta quando um agente (`claude`, `pi`, `codex`, `opencode`, `antigravity`) encerra e o PTY retorna ao prompt do shell (`zsh`, `bash`, `fish`, `sh`, `nu`, `dash`), ignorando labels residuais (`cockpit.agent`, `cockpit.state`, títulos `π - ...`) para que shells comuns nunca apareçam como agentes trabalhando.
  - **Normalização de terminais encerrados (`state == "finished"`)**: terminais finalizados retornam imediatamente `AttentionState::Done` (ou `Error` se `exit_code != 0`), nunca ficando presos como `Blocked` (`PERMISSION`/`QUESTION`) ou `Working`.
  - Reconciliação automática do daemon a cada `5s` e refresh automático em `cockpit status` quando o snapshot tem `>= 2s`.
- **`src/supervisor/` (Pi-Durable FSM Supervisor)**:
  - **`hc.rs`**: Descoberta completa via `hc connect` (`terminal.list` com `foreground`, `labels`, `status`, `title`), resolução dinâmica de `$HOME/.local/bin/hc`, e exclusão automática de shells ociosos que retornaram ao prompt.
  - **`ollama.rs`**: Cliente cognitivo para Ollama (`lfm3:1.7b`, `num_gpu: 0`, `"think": false` para avaliação rápida em CPU em ~5s sem bloqueio por raciocínio longo, e `keep_alive: 0` para unload).
  - **`watcher.rs` & `guardrails.rs` & `state.rs`**:
    - Máquina de estados determinística (`SemiAuto`, `AlertOnly`, `Paused`) com persistência em `~/.local/state/cockpit/supervisor_state.json` (incluindo `zero_active_since` e `model_loaded`).
    - Auto-resposta usa `hc send --force` para responder diálogos de terminais em estado `blocked` e limpa os labels `cockpit.state=working`, `cockpit.kind=""`, `cockpit.msg=""`.
    - Guardrails determinísticos bloqueiam comandos destrutivos (`git push --force`, `rm -rf`, `DROP TABLE`, etc.) no output ou no payload, convertendo automaticamente para `ALERT_OPERATOR` (`Urgency::Critical`), e normalizam respostas de prompts `(y/n)` / `[y/n]` para `"y"`.
    - **Zero-Idle Unload**: quando `active_sessions == 0`, o modelo `lfm3:1.7b` é descarregado imediatamente da RAM no Ollama e o status passa para `Supervisor: Idle`.
  - **`tui.rs`**: Probe ao vivo do servidor Ollama (`/api/tags` + `/api/ps`) expondo `llm_connected`, `model_installed`, `model_loaded` e `llm_status_detail` no `DesktopModSnapshot` e no `cockpit doctor`.
- **`src/home.rs`**:
  - Gerenciamento de **Projetos em Desenvolvimento** e **Tarefas do Dia** (`~/.local/share/cockpit/daily_tasks.json`) via `cockpit home --json` e `cockpit task <list|add|toggle|remove|clear-done>`.
- **`src/usage.rs`**:
  - Integração com **`ai-usagebar`** / **`ai-usagebar-tui`** (`cockpit usage [--json] [--detect]`): localiza os binários no `PATH` e diretórios padrão (`~/.cargo/bin`, `~/.local/bin`, `/usr/local/bin`), coleta `vendors --json` e `usage --json` com timeout, suporta `--detect` (`ai-usagebar detect`), e retorna `UsageBarSnapshot` com fallback gracioso caso o binário não esteja instalado.
- **`assets/herdr-app/` (Herdr Desktop Layout & Mod)**:
  - **`layouts/cockpit.json`**: Layout em 3 zonas (`cockpit-pill` no topo, `sidebar` + `cockpit-fleet` à esquerda, terminal principal no centro, gaveta unificada `cockpit-inspector` à direita para `Home` / `Inspector` / `Doctor`, e `cockpit-status` no rodapé).
  - **`mods/cockpit/main.js`**:
    - Alinhado visualmente com o `DESIGN.md` da Landing Page e compatível com múltiplos temas GPUI (sem uso de `"transparent"` ou `.truncate()` em `v_flex`).
    - Seleção de cards em `COCKPIT FLEET` funciona tanto para abas visíveis quanto para agentes em background (`hc run --bg` / `herdr-core`, `pane === null`): ao clicar em uma linha da Fleet ou em `Focus Terminal` no Inspector, se a tentativa inicial de foco (`pane.focus` / `agent.focus`) não abrir/focar o terminal em ~200 ms, `focusOrAttachTerminal` aciona automaticamente `tab.new` com `herdr-core attach <target>` (preferindo `term.name`, ex.: `lore-agy`, com fallback para o endereço e debounce de 1200 ms). O botão `Attach Tab` também usa `herdr-core attach <target>`, abas de attach abertas são resolvidas em `findPaneForTerminal` / `selectedAddress()` para o agente original e filtradas como clientes de attach em `src/herdr.rs`, `src/supervisor/hc.rs` e `isPlainShellRow`.
    - Indicador de conexão do modelo local na barra superior (`● LLM: lfm3:1.7b (ollama · PRONTO / EM RAM)`), na Home e no Doctor.
    - **Popover `Uso` (`ai-usagebar`)**: botão `Uso` exclusivo na barra inferior direita (`cockpit-status`, imediatamente à esquerda de `Home`) que abre o card compacto `AI Usage` (`renderUsagePopover`) ancorado no canto inferior direito — exibindo apenas uma conta por vez com sub-abas horizontais por provedor, barras segmentadas de uso e tempo decorrido, countdown/pacing e ações `↻` / `⋯` (`Detectar Contas`, `Abrir TUI`, `Login`) / `×`, com fallback direto para `ai-usagebar` caso o CLI `cockpit` do host ainda não esteja atualizado.
- **`landing-page/` (Ignorada no Git via `.gitignore`)**:
  - Projeto Next.js/React local em `/home/ubuntu/.t3/projects/cockpit/landing-page` com `DESIGN.md` e componentes interativos (`HeroControlPlane.tsx`, Arquitetura em 3 Camadas, Zero-Idle & Proteção de VRAM, Autonomia com Guardrails).

### Atualizações (Fleet Auto-Attach `herdr-core`, Uso / `ai-usagebar`, GPU toggle, RAM/VRAM e guardrails de Action Cards)
- **Fleet Auto-Attach (`herdr-core attach <target>`)**: `focusOrAttachTerminal` em `main.js` tenta focar o pane/agente (`pane.focus` / `agent.focus`) e, se em 200 ms nenhum pane existir e o foco não estiver no agente (ex.: sessões `herdr-core` sem pane como `lore-agy`), abre uma aba executando `herdr-core attach <target>` (usando `term_name` / `address`, com debounce de 1200 ms). `mergeFleetRows` inclui terminais vivos de `terminals()` (Loop 4), resolve `term_name`/`attach_target`/`pane` (inclusive via abas wrapper de `attach`), `selectedAddress()` remapeia wrapper tabs de volta ao agente alvo, e `src/herdr.rs`, `src/supervisor/hc.rs` e `isPlainShellRow` filtram processos `herdr-core attach` / `hc attach` para evitar entradas fantasma/duplicadas.
- **Uso (`ai-usagebar`)**: Novo módulo `src/usage.rs` e subcomando `cockpit usage [--json] [--detect]`; no mod (`main.js`), `Uso` fica apenas na barra inferior direita (`cockpit-status`) à esquerda de `Home` e abre o popover compacto `AI Usage` ancorado na base direita com sub-abas por provedor (uma conta por vez). Refresh automático a cada 60s enquanto aberto.
- **GPU toggle**: `cockpit supervisor gpu <on|off> [--no-warm]` persiste em `~/.local/state/cockpit/supervisor_gpu` (precedência: env `COCKPIT_ALLOW_GPU` > arquivo > toml), descarrega o modelo (`keep_alive: 0`) e faz warm-load. A pílula `num_gpu` da barra superior é clicável. `supervisor watch` (daemon) lê a config só ao iniciar — reinicie após alternar.
- **RAM/VRAM**: `DesktopModSnapshot` ganhou `allow_gpu`, `llm_ram_bytes`, `llm_vram_bytes` (Ollama `/api/ps`: `size - size_vram` / `size_vram`). O rótulo aparece na pílula **LLM** (a pílula Supervisor não tem espaço fixo suficiente para `1.1 GB VRAM · 0.2 GB RAM`).
- **Action Cards**: `guardrails::enforce_blocked_policy` — só agentes `blocked`/`waiting` geram AUTO_REPLY/ALERT; working → NoOp (resumo atualizado). `Critical` só por detecção determinística de comando destrutivo nas últimas linhas (prompt) ou no payload; urgência crítica vinda do LLM é rebaixada a `Normal`; alerta sem prompt pendente não leva payload; cards de agentes que deixaram de estar blocked são limpos no tick seguinte. `contains_forbidden_payload` agora respeita fronteira de palavra (`confirm ` ≠ `rm `).
- Atenção: `~/.cargo/bin/cockpit` pode sombrear `~/.local/bin/cockpit` no PATH.

---

### Atualização — LLM do Supervisor em container `llama.cpp` (`6a38eee`)
- **`src/supervisor/container.rs`**: detecção de runtime `kiri` → `podman` → `docker` (override `[llm_container] runtime` / `COCKPIT_CONTAINER_RUNTIME`); container nomeado `cockpit-supervisor-llm` (`ghcr.io/ggml-org/llama.cpp:server`, GPU: `:server-cuda` + `--gpus all` / CDI no podman) em `127.0.0.1:11435`; `start` recria o container se o label `cockpit.ngl` difere do `-ngl` pedido; RAM via `<rt> stats` (`MemUsage`); espera `/health` 200.
- **Modelo GGUF** (precedência): `llm_container.model_path` → `llm_container.hf_repo` (`-hf`, cache em `~/.cache/cockpit/llama.cpp` com `LLAMA_CACHE`) → blob do `ollama_model` no store do Ollama (`$OLLAMA_MODELS`, `~/.ollama/models`, `/usr/share/ollama/.ollama/models`).
- **Modo**: `[llm_container] mode = auto|on|off` (env `COCKPIT_LLM_CONTAINER`); `auto` ativa quando há runtime no PATH → backend efetivo `llama-cpp`. `off` mantém o caminho Ollama legado intacto.
- **Ciclo de vida explícito**: ticks nunca sobem o container; com ele parado o watcher pula a avaliação LLM (sem marcar output como avaliado). `cockpit supervisor load` sobe + espera healthy; `unload` faz `stop -t 2`; auto-stop só após `idle_unload_secs` sem sessões. `supervisor gpu on|off` recria o container se estiver rodando.
- **Snapshot/doctor**: `llm_managed`, `llm_runtime`, `llm_container_state` (`missing|stopped|running`).
- **Mod**: botão único `▶ Load LLM` ⇄ `⏻ Unload LLM` (`◌ Carregando…` durante o load, timeout 300 s); pílula `○ DESATIVADO` / `◌ INICIANDO…` / `● llama.cpp · X RAM`; `⚡ Poll Now` faz load antes se parado.
- **Validado no `ai-box`** (Docker 29, modelo de teste SmolLM2-135M via `hf_repo`, HOME temporário): load 7,6 s (1,8 s com container existente), chat completions OK, unload → `Exited (0)`, recriação reaproveita o cache. **Não validado**: GGUF do `lfm3:1.7b` vindo do store do Ollama, GPU, kiri/podman.

---

### Atualização — M1 seleção de modelo (`1e045db` backend, commit do mod em seguida)
- Plano/spec: `docs/superpowers/plans/2026-11-14-m1-model-selection.md`, `docs/superpowers/specs/2026-11-14-m1-model-selection-design.md`.
- Backend: `gguf.rs` (leitor de cabeçalho), `models.rs` (varredura Ollama/HF/llama.cpp/LM Studio/pastas + filtro), `config_write.rs` (`toml_edit`), `ModelId` + label `cockpit.model_id`, CLI `cockpit supervisor model list|set|filter|reset`. `supervisor.toml` parcial agora é aceito.
- Mod: card "Supervisor model" na gaveta direita, aberto pela pílula LLM (`ck-llm-pill`) ou pelo card LLM do Inspector (`ck-inspector-llm-card`).
- Validado no `ai-box`: 121 testes; CLI com GGUFs sintéticos; smoke Docker com SmolLM2 135M/360M (troca com container rodando recria em ~2,3 s; troca com container parado só grava; label `cockpit.model_id` confere). **Falta validar o card no Herdr Desktop do host.**
- Swarm do Antigravity travou 2× sem escrever código (workers ficaram `waiting`, coordenador encerra o turno); Hydrogen solo funcionou mas encerra antes de validar — o coordenador validou.

---

### Atualização — correções do teste no host (`50db329` + Task 6)
- Plano: `docs/superpowers/plans/2026-11-14-host-test-fixes.md`.
- O que mudou:
  - agy detectado como agente (`AgentKind::Antigravity`);
  - avaliação LLM com limite (`-np 1`, `maxLength`, `max_tokens 384`) e erro visível (`last_eval_error`);
  - card **Pending prompt** + `cockpit supervisor respond <t> approve|deny` (Enter/Esc; destrutivo recusado);
  - fallback de runtime (`~/.local/state/cockpit/container_runtime`);
  - ícone do agente na Fleet;
  - clique em sessão em background sempre abre a aba de attach se não houver painel; sessão encerrada abre o histórico (`herdr-core read | less -R`).
- **Validar no host:**
  - clicar numa sessão agy viva (`hc run --bg`) deve abrir a aba de attach. A linha `attach: …` no Inspector mostra o caminho; se não abrir, anotar essa linha;
  - card Pending prompt em sessão PERMISSION;
  - repetir o teste multi-agente com o lfm3 (a avaliação deve terminar ou mostrar o erro).

---

### Atualização — 2026-11-15: rename `tchelvis`, distribuição (M5a) e benchmark CPU vs GPU
- **Rename** (`028bd5f`): o binário e o crate viraram `tchelvis` 0.2.0, junto com dirs `~/.config|.local/share|.local/state|.cache/tchelvis`, mod e layout do Herdr `tchelvis` (nós `tchelvis-*`), container `tchelvis-supervisor-llm` (labels `tchelvis.*`), hooks e systemd.
  - Variáveis: só `TCHELVIS_*`; as `COCKPIT_*` não são mais lidas.
  - **Mantidos:** labels hc `cockpit.*` e chaves de shared-state `cockpit.*` (protocolo interno).
  - **Sem migração** (decisão do usuário): instalação limpa.
- **Licença:** dupla MIT OR Apache-2.0 (`LICENSE-MIT`, `LICENSE-APACHE`).
- **Distribuição** (`425bc0b`, `2534d5f`):
  - `.github/workflows/release.yml`:
    - testes;
    - build musl estático `x86_64`/`aarch64` (runner `ubuntu-24.04-arm`);
    - publish: tag `v*` → stable (notas do CHANGELOG); push na `main` → pré-release `nightly` recriada com `commit <sha>`.
  - `tchelvis update [--channel stable|nightly] [--check] [--no-setup] [--force]`:
    - baixa, confere o `sha256` e troca o binário atomicamente;
    - roda `setup`, para e reinicia `tchelvis daemon` / `supervisor watch`;
    - lembra o canal em `~/.local/state/tchelvis/update_channel`.
  - `install.sh` (POSIX). `docs/RELEASING.md` explica como cortar um stable.
  - Validado: workflow verde, instalação limpa e update `2534d5f → ee71994` testados de ponta a ponta no `ai-box`.
  - Nota: a API do GitHub tem cache de 60 s, então `update` logo após um push pode dizer "Already up to date"; esperar 1 min.
  - A conta do GitHub ficou bloqueada por cobrança uma vez (resolvido pelo usuário).
- **GPU + benchmark** (`d99e3fb`, `ee71994`):
  - `[llm_container] gpu_backend = auto|cuda|vulkan|rocm` (`TCHELVIS_GPU_BACKEND`):
    - auto: `/dev/nvidia0` ou `nvidia-smi` → CUDA (`server-cuda`), `/dev/dri/renderD*` → Vulkan (`server-vulkan`), senão none;
    - ROCm (`server-rocm`) só se forçado.
    - O host do usuário tem GPU **AMD** → Vulkan.
  - `tchelvis supervisor bench [--runs N] [--json] [--last] [--progress]`:
    - mede CPU e depois GPU com o pedido real do supervisor (timings do llama-server, mediana de 3 após aquecimento);
    - restaura o estado anterior do container;
    - lock contra execução dupla;
    - resultado em `~/.local/state/tchelvis/bench_last.json`.
  - Card "CPU vs GPU benchmark" na aba Doctor do mod.
  - Validado no `ai-box` (sem GPU): linha da CPU com números, "No GPU detected" e restauração do estado.
- **M1c ocultar/apagar modelos** (`model hide|unhide|rm`):
  - `[llm_container] hidden_models` em `supervisor.toml` (`config_write::write_hidden_models`); `model list --json` expõe `hidden: bool` e `user_hidden_count`; `model list --all` mostra os ocultos pelo usuário.
  - `tchelvis supervisor model rm <id> [--yes] [--json]`: planejador puro (`model_delete.rs`) trata arquivos planos (+ shards), cache HF (`models--*`, preservando blobs compartilhados e podando o diretório só quando não resta `.gguf`), e Ollama (`ollama rm` ou `DELETE /api/delete`). Recusa modelos protegidos (efetivo, `fallback_model_id`, ou `tchelvis.model_id` no container em execução) e recusa stdin não-interativo sem `--yes`.
  - Mod: ações `Hide`/`Show` e `Delete` (duplo clique em 4 s: `Delete <size>?`) nas linhas elegíveis do card `Supervisor model`.
  - **Validar no host:** fluxo visual de `Hide`/`Show` e `Delete` no card do Herdr Desktop.
- **Fleet child tree** (`main.js`): terminais filhos aninhados sob o pai (`├`/`└`, badge `⚠ N filho(s)` no pai, grupo sobe para "Needs" se algum filho estiver blocked); **validar no host** (clicar num filho seleciona o filho; filho blocked puxa o grupo para cima).
- **Planos e specs** em `docs/superpowers/{specs,plans}/` (M1, host-test fixes, M5a, bench, M1c, fleet child tree, read-only fast path).
- **Execução:** Gemini 4 Hydrogen (medium) solo via `delegate_task`; o coordenador revisa, valida e faz commit.
  - O swarm do Antigravity travou 2× (workers parados), por isso não é usado.
  - O Hydrogen às vezes encerra antes de validar; o coordenador sempre revalida.

---

### Atualização — 2026-11-15 (tarde): attach, supervisor endurecido pelos testes 2–8, Fleet em árvore
- **Attach na Fleet** (`67fe16f`, `114c4ce`): a aba de attach usa o **endereço** (`hc attach t11g`), nunca o nome (terminais criados por agentes não têm nome no core → `not_found`). O clique no card só foca um pane que esteja na lista atual; senão abre o attach direto (`planFleetClick`). **Validado no host.**
- **CI** (`9a09014`): o passo `Mod tests` (Node 24) exige os arquivos (`tests/mod/*.test.mjs`), não a pasta. Entre `67fe16f` e `9a09014` nenhum nightly saiu — sempre conferir o run após o push.
- **Testes do mod em Node**: `node --import ./tests/mod/stub-hooks.mjs --test tests/mod/*.test.mjs` (stubs de `herdr`/`gpui-*`); funções puras exportadas de `main.js`.
- **Pílula LLM** (`855f266`): modelos do cache HF mostram o nome do snapshot, não o hash do blob; nome encurtado no meio (28 chars).
- **Supervisor — política de resposta** (`guardrails.rs`), resultado de 8 rodadas de teste no host (relatórios em `.work/teste-supervisor-*` no host):
  - `sanitize_auto_reply`: AUTO_REPLY só digita `y`/`n` (só em prompt sim/não), número de menu ou Enter; texto livre vira alerta. Prompt com `kind=question` nunca é auto-respondido (`HcSession.kind`).
  - `enforce_blocked_policy`: bloqueado + `NO_OP` com prompt visível vira alerta `Normal`.
  - Fila (`watcher.rs`): bloqueados primeiro (mais antigo primeiro), `max_evals_per_tick` (padrão 1).
  - **Regra fixa de leitura** (`readonly_fast_path_with_msg`, spec `2026-11-15-readonly-fast-path-design.md`): antes e sem a LLM aprova `ls/cat/head/tail/wc/pwd/rg/grep/git status|diff|log|show` e `Read` de um arquivo, sem metacaracteres nem caminhos sensíveis; comando vem do `status.msg` do hc (`approve Bash: …`, Claude Code), da linha do prompt (`Allow bash tool: … ? [y/N]`) ou da linha acima; tecla `1` (menu "1. Yes") ou `y`. Opções 2/3 do Claude (always allow / auto mode) nunca. `WatcherEngine::apply_decision` é compartilhado pela regra e pela LLM.
  - **Teste 8** (nightly `c8a0222`): todos os cenários corretos; seguros em 7–18 s sem LLM; etapa destrutiva da `sequencia` alcançada e alertada; nenhum destrutivo recebeu `y` em 8 rodadas.
  - Telas reais: só o **Claude** pede permissão nas configurações do usuário (opencode `autoaccept`, pi sem barreira, agy `auto`); o Claude não pede para leitura. Tela real em `src/supervisor/fixtures/claude_permission_bash_touch.txt` (caminho anonimizado).
- **M1c** (Hydrogen, revisado): ocultar/apagar modelos; acrescentada recusa de arquivo não-`.gguf` fora do layout HF (`b178f1b`).
- **Fleet em árvore** (Hydrogen, `403a1df`, `4a613db`): spec `2026-11-15-fleet-child-tree-design.md`.
- **ROADMAP:** M1c, M1d (benchmark de qualidade de decisão), M6 (modo delegador: supervisor acorda o coordenador quando um worker volta ao prompt; referência: contrato de delegação do T3 Code).

### Atualização — 2026-11-16: teste 10, `v0.2.0`
- **`45d276a`** (resultado do teste 9): a LLM nunca aprova pedido de permissão (`sanitize_auto_reply` recebe o `status.msg` do hc; só a regra fixa de leitura aprova); toda auto-resposta enviada vai para `~/.local/state/tchelvis/auto_replies.jsonl` com `source` (`fixed_rule|llm`); `hc send` com falha vira erro no card e rótulo `AUTO_REPLY_FAILED` (teste unitário `failed_hc_send_records_error_and_sets_auto_reply_failed_label`, não dá para provocar por fora).
- **Teste 10** (nightly `45d276a`, relatório do host): **passou**. 27 auto-respostas, todas `fixed_rule`, nenhuma da LLM; destrutivos (`rm -rf`, `curl | sudo sh`, `git push --force` com injeção, `git reset --hard`) e escrita (`Edit`, `touch` no Claude real) viraram alerta; Claude real recebeu `1` só no `cat` e continuou em `manual mode`; o jsonl sobreviveu ao `hc kill`/`hc rm` dos 38 terminais.
  - Decisão mantida: a regra fixa continua aprovando leituras triviais também no menu do Claude (spec `2026-11-15-readonly-fast-path-design.md`).
- **`e394340`**: quando uma proteção transforma a resposta sugerida pela LLM em alerta, o card começa com `[Sugestão da LLM não enviada]` (antes dizia "AUTO_REPLY aplicada porque…"); caracteres CJK soltos pelo modelo pequeno (`git 清`) são removidos do texto (`tidy_operator_reason` em `guardrails.rs`).
- **`v0.2.0`** (`a6a81d1`, `chore(release): v0.2.0`): CHANGELOG completado (regra fixa, jsonl, M1c, Fleet em árvore, política de auto-resposta, attach por endereço) e movido para `## [0.2.0]`; tag enviada; release estável com binários `x86_64`/`aarch64`, `.sha256`, `SHA256SUMS` e notas. `install.sh --channel stable` validado no `ai-box` (`tchelvis 0.2.0 (stable a6a81d1)`). No host: `tchelvis update --channel stable`.
- Abertos do teste 10: latência do 1º lote ~30 s (teste 9: 10–13 s; lotes seguintes 12–20 s), não investigada; `supervisor status` não mostra o histórico de terminais fechados (só o jsonl).

---

## 2. Histórico de Commits Recentes (`main`)

- `a6a81d1` — `chore(release): v0.2.0` (tag `v0.2.0`)
- `e394340` — card de alerta não parece resposta enviada; remove ruído CJK
- `45d276a` — LLM nunca aprova permissão; `auto_replies.jsonl`; `AUTO_REPLY_FAILED`
- `d974d9b` — handoff da sessão de 2026-11-15
- `164a947` / `a650bd6` — fast path lê o pedido do `status.msg` do hc (menu real do Claude Code)
- `c8a0222` / `390dfcc` — fast path: comando na linha do prompt; acima do prompt; comandos entre crases
- `74719db` / `4a613db` / `403a1df` — Fleet em árvore (+ `cargo fmt`)
- `aa72533` — `feat(supervisor): read-only fast path approves trivial commands without the LLM`
- `2d405d0` — nunca auto-responder perguntas; `y`/`n` só em prompt sim/não; bloqueado + `NO_OP` alerta
- `114c4ce` — clique no card da Fleet anexa se não houver pane vivo
- `9a09014` — CI: arquivos de teste do mod explícitos (Node 24)
- `855f266` — pílula LLM: nome do snapshot HF em vez do hash
- `b178f1b` / `3c92619` … `37b2799` — M1c ocultar/apagar modelos
- `ef1bbae` / `4e8a0c1` — fila com bloqueados primeiro; AUTO_REPLY só teclas simples
- `67fe16f` — attach pelo endereço
- `ee71994` — `feat(mod): CPU vs GPU benchmark card in the Doctor tab`
- `d99e3fb` — `feat(supervisor): GPU backend detection (CUDA/Vulkan/ROCm) and supervisor bench CPU vs GPU`
- `2534d5f` — `ci,docs: release workflow (stable tags + nightly per push) and tchelvis install/update docs (M5a B3)`
- `425bc0b` — `feat(update): tchelvis update, install.sh and daemon restart (M5a B2)`
- `028bd5f` — `feat!: rename cockpit to tchelvis; version metadata; dual MIT/Apache-2.0 license (M5a B1)`
- `e432539` / `50db329` — correções do teste no host (agy, avaliação LLM limitada, Pending prompt, fallback de runtime, attach de sessões em background)
- `6d1f86b` / `1e045db` — M1 seleção de modelo (backend + card no mod)
- `6a38eee` — `feat(supervisor,mod): managed llama.cpp container with explicit Load/Unload lifecycle`
- `27ad04e` — `fix(fleet,herdr): auto-attach unattached herdr-core terminals on click`
  - Auto-attach via `herdr-core attach <target>` em `focusOrAttachTerminal` (`main.js`) com verificação de foco após `200 ms` e debounce de `1200 ms`; `mergeFleetRows` recebeu Loop 4 (`terminals()`) + `findPaneForTerminal`, `selectedAddress()` recebeu `resolveAttachedTargetAddress`, e clientes `attach` agora são filtrados em `src/herdr.rs`, `src/supervisor/hc.rs` e `isPlainShellRow`.
- `69c6d19` — `feat(mod): make AI Usage compact bottom-right popover with provider sub-tabs`
  - Adicionado `cockpit usage [--json] [--detect]`; botão `Uso` exclusivo na barra inferior direita abrindo o popover compacto `AI Usage` com sub-abas horizontais por provedor.
- `2ce7dc3` — `feat(supervisor,mod): GPU toggle, LLM RAM/VRAM label, and blocked-only critical action cards`
  - Pill da LLM mostra o consumo real (`● LLM: lfm3:1.7b (ollama · 1.2 GB RAM)` / `VRAM` quando na GPU) via `/api/ps` (`llm_ram_bytes`, `llm_vram_bytes`, `allow_gpu` no snapshot).
  - Pill `num_gpu: 0 (CPU)` ↔ `num_gpu: all (GPU)` clicável; CLI `cockpit supervisor gpu <on|off> [--no-warm]`; persistido em `~/.local/state/cockpit/supervisor_gpu` (precedência: env `COCKPIT_ALLOW_GPU` → arquivo → TOML). Troca faz unload + warm-load. Padrão continua CPU (Zero-VRAM).
  - `enforce_blocked_policy` (`src/supervisor/guardrails.rs`): só agentes realmente `blocked` geram Action Card; `Critical` só para comandos destrutivos detectados deterministicamente (últimas 8 linhas / payload); urgência `Critical` vinda do LLM é rebaixada para `Normal`; sem prompt pendente → sem payload; cards obsoletos são limpos no tick seguinte. `contains_forbidden_payload` respeita limites de palavra (`confirm ` não casa mais com `rm `).
- `7a8c17b` — `fix(herdr-mod): pin top-bar mode switcher and simplify supervisor pill`
  - Pill do supervisor mostra só `Supervisor: {Idle|Paused|Evaluating|Watching (N)}` (`min_w(196)`); clusters laterais `flex_1().min_w(0)` e central `flex_none()` → botões Semi-Auto / Alert Only / Paused / Poll Now / Unload LLM fixos no centro.
- `3adf6b3` — `fix(fleet,supervisor): filter idle shells after agent exit, normalize finished rows, and stop idle LLM evals`
- `0c5c727` — `fix(supervisor,mod): fix background agent selection, hc send --force, and lfm3 think:false`
- `d414a7a` — `fix(cockpit): fix provider status field, hc connect discovery, hooks PATH, and LLM connection UI`

---

## 3. Comandos Rápidos para a Próxima Sessão

### Instalar / atualizar no Host (Release / Nightly)
No terminal da máquina **host**:
```bash
# one time (old cockpit → tchelvis, clean install)
cockpit uninstall --purge; docker rm -f cockpit-supervisor-llm 2>/dev/null; podman rm -f cockpit-supervisor-llm 2>/dev/null; hash -r
curl -fsSL https://raw.githubusercontent.com/fabriciof807/Tchelvis-Supervisor/main/install.sh | sh -s -- --channel nightly
# afterwards
tchelvis update
```
> ⚠️ Se houver variáveis `COCKPIT_*` nos arquivos de shell (`~/.bashrc`, `~/.zshrc`, etc.), renomeie-as para `TCHELVIS_*` (ex.: `TCHELVIS_LLM_CONTAINER`, `TCHELVIS_ALLOW_GPU`, `TCHELVIS_LLM_API_KEY`).
> ⚠️ Se `which -a tchelvis` listar `~/.cargo/bin/tchelvis` antes de `~/.local/bin/tchelvis`, ele é um binário antigo e sombreia o novo (o mod chama `tchelvis` puro). Substitua-o ou remova-o.

### Alternar CPU/GPU do modelo do Supervisor
```bash
tchelvis supervisor gpu on    # num_gpu: 99 (offload total) + warm-load
tchelvis supervisor gpu off   # num_gpu: 0 (Zero-VRAM, padrão)
```
> ⚠️ O daemon `tchelvis supervisor watch` só lê a config ao iniciar — reinicie-o após alternar.

### LLM em container (`llama.cpp`)
```bash
tchelvis supervisor load      # sobe/inicia tchelvis-supervisor-llm e espera /health
tchelvis supervisor unload    # para o container (0 B RAM/VRAM)
tchelvis supervisor status --json | jq '{llm_managed, llm_runtime, llm_container_state, llm_status_detail}'
TCHELVIS_LLM_CONTAINER=off tchelvis supervisor status --json   # volta ao Ollama legado
```
> `supervisor.toml` aceita só as chaves que você quiser definir (o resto usa o padrão).

### Rodar Testes e Build no Container (`ai-box`)
```bash
cargo test --quiet
node --check assets/herdr-app/mods/tchelvis/main.js
cargo build --release   # binário local em target/release/tchelvis; no host use `tchelvis update`
```

### Verificar Diagnóstico Completo (Hooks, Herdr Mod, Providers e Ollama)
```bash
tchelvis doctor
tchelvis supervisor status --json
```

---

## 4. Próximos Passos Sugeridos
1. **Próxima sessão: M1b — modelo fallback** (decisão do usuário, 2026-11-16). Ver `ROADMAP.md` § M1b.
   - Começar pelo brainstorming/spec: definir "parou de funcionar" (load falhou, health check falhou, erros repetidos de avaliação/JSON), quando volta ao modelo principal, e como o mod mostra que o fallback está ativo.
   - Já existe: chave `[llm_container] fallback_model_id` (`src/supervisor/container.rs:106`), usada hoje só para proteger o modelo no `model rm` (`src/supervisor/mod.rs:417`); `last_eval_error` por agente; spec M1 (`docs/superpowers/specs/2026-11-14-m1-model-selection-design.md`, non-goals).
   - Fluxo de sempre: spec → plano → Hydrogen em worktree → revisão → push com OK.
2. **Validar no host** (com `v0.2.0`, reiniciar o Herdr Desktop):
   - Fleet em árvore: filhos recuados com `├`/`└`, `⚠ N` no pai, grupo vai para "Needs" se um filho bloqueia, clique no filho seleciona o filho.
   - Card de modelos: Ocultar/Mostrar e Apagar (segundo clique `Delete <size>?`); `ollama rm` real nunca foi testado.
   - Texto novo do card de alerta (`[Sugestão da LLM não enviada]`).
3. **Pendências do supervisor:**
   - Latência do primeiro lote (~30 s no teste 10): registrar latência/decisão por avaliação no log (`tchelvis logs` não mostra os ticks disparados pelo app) e investigar.
   - Visibilidade: terminais avaliados (decisão + horário) no `supervisor status`, inclusive os já fechados (hoje só no `auto_replies.jsonl`).
   - A LLM ainda recebe a tela inteira e pode confundir um prompt antigo com o atual.
   - Justificativas cortadas em 200 caracteres pelo schema (cosmético).
4. **Depois (ver `ROADMAP.md`):** M1d benchmark de qualidade de decisão; M2 ícones; M3 layout compacto; M4 Inspector multi-terminal; M5b AUR/Homebrew; M6 modo delegador (ideia).
5. **Pendências pequenas:** avisos antigos do clippy (`guardrails.rs:26`, `models.rs`, `hc.rs`, `container.rs`, testes do `watcher.rs`); aviso no `update` sobre o cache de 60 s da API; botão GPU reiniciar o `supervisor watch`.
