# Handoff — Cockpit (Unified Fleet Control Plane + Durable Supervisor)

**Data da última atualização:** 2026-11-14  
**Repositório:** `https://github.com/fabriciof807/cockpit` (branch `main`)  
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
- **`assets/herdr-app/` (Herdr Desktop Layout & Mod)**:
  - **`layouts/cockpit.json`**: Layout em 3 zonas (`cockpit-pill` no topo, `sidebar` + `cockpit-fleet` à esquerda, terminal principal no centro, gaveta unificada `cockpit-inspector` à direita para `Home` / `Inspector` / `Doctor`, e `cockpit-status` no rodapé).
  - **`mods/cockpit/main.js`**:
    - Alinhado visualmente com o `DESIGN.md` da Landing Page e compatível com múltiplos temas GPUI (sem uso de `"transparent"` ou `.truncate()` em `v_flex`).
    - Seleção de cards em `COCKPIT FLEET` funciona tanto para abas visíveis quanto para agentes em background (`hc run --bg`, `pane === null`), com botões `Attach Tab` (`hc attach`), `Focus Terminal` / `Peek` e `Kill` (`hc kill`) no Inspector, além de `Clear Done` (`hc rm`) na aba `Done`.
    - Indicador de conexão do modelo local na barra superior (`● LLM: lfm3:1.7b (ollama · PRONTO / EM RAM)`), na Home e no Doctor.
- **`landing-page/` (Ignorada no Git via `.gitignore`)**:
  - Projeto Next.js/React local em `/home/ubuntu/.t3/projects/cockpit/landing-page` com `DESIGN.md` e componentes interativos (`HeroControlPlane.tsx`, Arquitetura em 3 Camadas, Zero-Idle & Proteção de VRAM, Autonomia com Guardrails).

### Atualizações (GPU toggle, RAM/VRAM e guardrails de Action Cards)
- **GPU toggle**: `cockpit supervisor gpu <on|off> [--no-warm]` persiste em `~/.local/state/cockpit/supervisor_gpu` (precedência: env `COCKPIT_ALLOW_GPU` > arquivo > toml), descarrega o modelo (`keep_alive: 0`) e faz warm-load. A pílula `num_gpu` da barra superior é clicável. `supervisor watch` (daemon) lê a config só ao iniciar — reinicie após alternar.
- **RAM/VRAM**: `DesktopModSnapshot` ganhou `allow_gpu`, `llm_ram_bytes`, `llm_vram_bytes` (Ollama `/api/ps`: `size - size_vram` / `size_vram`). O rótulo aparece na pílula **LLM** (a pílula Supervisor não tem espaço fixo suficiente para `1.1 GB VRAM · 0.2 GB RAM`).
- **Action Cards**: `guardrails::enforce_blocked_policy` — só agentes `blocked`/`waiting` geram AUTO_REPLY/ALERT; working → NoOp (resumo atualizado). `Critical` só por detecção determinística de comando destrutivo nas últimas linhas (prompt) ou no payload; urgência crítica vinda do LLM é rebaixada a `Normal`; alerta sem prompt pendente não leva payload; cards de agentes que deixaram de estar blocked são limpos no tick seguinte. `contains_forbidden_payload` agora respeita fronteira de palavra (`confirm ` ≠ `rm `).
- Atenção: `~/.cargo/bin/cockpit` pode sombrear `~/.local/bin/cockpit` no PATH.

---

## 2. Histórico de Commits Recentes (`main`)

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
1. Validar no host com o binário `2ce7dc3` (prompt do teste multi-agente `sim-build` / `sim-safe` / `sim-danger` está na conversa anterior e pode ser reenviado):
   - Barra superior: botões centrais fixos enquanto a pill passa por `Idle` → `Watching (3)` → `Evaluating`; consumo de RAM/VRAM aparecendo na pill da LLM.
   - Toggle `num_gpu` na pill: carregamento na GPU real (não testado no container, sem GPU) e VRAM refletida na pill.
   - `sim-safe` → `AUTO_REPLY` (~5s); `sim-danger` → Action Card `Critical`; agente apenas `WORKING`/aguardando subagente → **nenhum** Action Card (regressão do falso `Critical` com `HooksError`).
   - Clique em cards de agentes em background abrindo o Inspector (`Approve`, `Ignore`, `Attach Tab`, `Kill`) e limpeza para `0 agents` / `Supervisor: Idle`.
2. Possível melhoria: o botão GPU do mod reiniciar automaticamente o `supervisor watch` (hoje precisa reinício manual para o daemon usar o novo `num_gpu`).
3. Evoluir a `landing-page/` ou adicionar novos fluxos de automação conforme os testes reais.
