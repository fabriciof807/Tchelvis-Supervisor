# Handoff — Cockpit (Unified Fleet Control Plane + Durable Supervisor)

**Data da última atualização:** 2026-11-14  
**Repositório:** `https://github.com/fabriciof807/Tchelvis-Supervisor` (público, branch `main`; antigo `cockpit` redireciona)  
**Binário instalado no container (`ai-box`):** `/home/ubuntu/.local/bin/cockpit`

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

## 2. Histórico de Commits Recentes (`main`)

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

### Sincronizar binário e mod do container (`ai-box`) para o Host
No terminal da máquina **host**:
```bash
incus file pull ai-box/home/ubuntu/.local/bin/cockpit ~/.local/bin/cockpit && \
chmod +x ~/.local/bin/cockpit && \
pkill -f "cockpit daemon" 2>/dev/null; pkill -f "cockpit supervisor watch" 2>/dev/null; \
~/.local/bin/cockpit setup --set-default-layout && \
which -a cockpit && \
~/.local/bin/cockpit supervisor status --json
```
> ⚠️ Se `which -a cockpit` listar `~/.cargo/bin/cockpit` antes de `~/.local/bin/cockpit`, ele é um binário antigo e sombreia o novo (o mod chama `cockpit` puro). Substitua-o ou remova-o.

### Alternar CPU/GPU do modelo do Supervisor
```bash
cockpit supervisor gpu on    # num_gpu: 99 (offload total) + warm-load
cockpit supervisor gpu off   # num_gpu: 0 (Zero-VRAM, padrão)
```
> ⚠️ O daemon `cockpit supervisor watch` só lê a config ao iniciar — reinicie-o após alternar.

### LLM em container (`llama.cpp`)
```bash
cockpit supervisor load      # sobe/inicia cockpit-supervisor-llm e espera /health
cockpit supervisor unload    # para o container (0 B RAM/VRAM)
cockpit supervisor status --json | jq '{llm_managed, llm_runtime, llm_container_state, llm_status_detail}'
COCKPIT_LLM_CONTAINER=off cockpit supervisor status --json   # volta ao Ollama legado
```
> ⚠️ `supervisor.toml` é parseado tudo-ou-nada: se faltar algum campo obrigatório, o arquivo inteiro é ignorado. Para definir `[llm_container] model_path`/`hf_repo`, mantenha o TOML completo.

### Rodar Testes e Build no Container (`ai-box`)
```bash
cargo test --quiet
node --check assets/herdr-app/mods/cockpit/main.js
cargo build --release && cp target/release/cockpit ~/.local/bin/cockpit && cp target/release/cockpit ~/.cargo/bin/cockpit && cockpit setup --set-default-layout
```

### Verificar Diagnóstico Completo (Hooks, Herdr Mod, Providers e Ollama)
```bash
cockpit doctor
cockpit supervisor status --json
```

---

## 4. Próximos Passos Sugeridos
1. **Próxima Sessão — Validar o container `llama.cpp` no host** (binário `6a38eee`):
   - `cockpit supervisor load` com o GGUF do `lfm3:1.7b` resolvido do store do Ollama do host (checar permissão de leitura se for `/usr/share/ollama`; senão usar `model_path`/`hf_repo`) e se o `llama-server` da imagem suporta a arquitetura LFM.
   - Saída JSON estruturada (`response_format: json_schema`) do llama.cpp nas avaliações reais (`sim-safe` → AUTO_REPLY).
   - Toggle GPU (`:server-cuda` + `--gpus all`) e runtime `kiri`/`podman` se instalados.
   - Botão `▶ Load LLM` ⇄ `⏻ Unload LLM` e pílula `DESATIVADO`/`INICIANDO…`/RAM na barra superior.
   - Considerar tornar todos os campos de `SupervisorConfig` `#[serde(default)]` (TOML parcial).
2. **Backlog / ordem das próximas entregas**: ver [`ROADMAP.md`](ROADMAP.md) (M0 → M1 seleção de modelo; M2 ícones → M3 layout compacto/ferramentas simples → M4 Inspector multi-terminal; M5a renomear binário para `tchelvis` + pipeline de release → M5b AUR/Homebrew Linux) e decisões D1–D4 (Linux only; sem imagem Docker do CLI por enquanto).
3. Validar no host com o binário mais recente (prompt do teste multi-agente `sim-build` / `sim-safe` / `sim-danger` está na conversa anterior e pode ser reenviado):
   - Barra superior: botões centrais fixos enquanto a pill passa por `Idle` → `Watching (3)` → `Evaluating`; consumo de RAM/VRAM aparecendo na pill da LLM.
   - Toggle `num_gpu` na pill: carregamento na GPU real (não testado no container, sem GPU) e VRAM refletida na pill.
   - `sim-safe` → `AUTO_REPLY` (~5s); `sim-danger` → Action Card `Critical`; agente apenas `WORKING`/aguardando subagente → **nenhum** Action Card (regressão do falso `Critical` com `HooksError`).
   - Clique em cards de agentes em background abrindo o Inspector (`Approve`, `Ignore`, `Attach Tab`, `Kill`) e limpeza para `0 agents` / `Supervisor: Idle`.
4. Possível melhoria: o botão GPU do mod reiniciar automaticamente o `supervisor watch` (hoje precisa reinício manual para o daemon usar o novo `num_gpu`).
5. Evoluir a `landing-page/` ou adicionar novos fluxos de automação conforme os testes reais.
