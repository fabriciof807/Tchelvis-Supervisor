# Handoff — Cockpit (Unified Fleet Control Plane + Durable Supervisor)

**Data da última atualização:** 2026-11-15  
**Repositório:** `https://github.com/fabriciof807/Tchelvis-Supervisor` (público, branch `main`; antigo `cockpit` redireciona)  
**Binário:** `tchelvis` (antes `cockpit`). Instalação/atualização via release: `install.sh` (1ª vez) e `tchelvis update` (canais `stable`/`nightly`; nightly atual = `ee71994`). Nenhum stable publicado ainda (`v0.2.0` só após validar o nightly no host, com OK do usuário).

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
- **Planos e specs** em `docs/superpowers/{specs,plans}/` (M1, host-test fixes, M5a, bench).
- **Execução:** Gemini 4 Hydrogen (medium) solo via `delegate_task`; o coordenador revisa, valida e faz commit.
  - O swarm do Antigravity travou 2× (workers parados), por isso não é usado.
  - O Hydrogen às vezes encerra antes de validar; o coordenador sempre revalida.

---

## 2. Histórico de Commits Recentes (`main`)

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
1. **Próxima sessão: validar no host** (nightly `ee71994`):
   - Se ainda tiver `cockpit`: roteiro de instalação limpa do §3. Senão, `tchelvis update`. Depois reiniciar o Herdr Desktop e escolher o layout `tchelvis`; renomear eventuais `COCKPIT_*` no `.zshrc`.
   - **Benchmark no Doctor** (Run benchmark): a linha `GPU (vulkan)` deve ter números na AMD. A 1ª vez baixa a imagem `server-vulkan` (GBs). Se falhar, anotar o erro da linha GPU (permissão de `/dev/dri`/grupo `video`, driver Vulkan na imagem).
   - **Correções do teste anterior:**
     - clique em sessão agy viva abre a aba de attach; se não abrir, anotar a linha `attach: …` do Inspector;
     - card **Pending prompt** (Approve/Deny/Open);
     - avaliação do lfm3 termina, ou mostra `⚠ LLM evaluation failed`;
     - fallback podman → docker.
   - **M1:** seletor de modelo pela pílula LLM ou pelo card LLM do Inspector.
2. **Se o nightly estiver bom:** cortar o primeiro stable `v0.2.0` (`docs/RELEASING.md`), **com OK do usuário** (push de tag).
3. **Próximas entregas (ver `ROADMAP.md`):**
   - M1b modelo fallback;
   - M2 ícones no lugar de emojis (o mapeamento do agy ainda usa "✨"/"✧");
   - M3 layout compacto / ferramentas simples (começa com auditoria D4);
   - M4 Inspector com vários terminais;
   - M5b AUR / Homebrew (Linux).
4. **Pendências pequenas:**
   - dois avisos de estilo do clippy em `models.rs`;
   - possível aviso no `update` sobre o cache de 60 s da API;
   - o botão GPU reiniciar o `supervisor watch` automaticamente.
