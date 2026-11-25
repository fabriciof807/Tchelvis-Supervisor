# Loop: limite de silêncio próprio para ferramenta aberta (`tool_stall_secs`)

**Data:** 2026-11-25. **Status:** aprovada pelo usuário em 2026-11-25 (as três recomendações); entrega A autorizada, B espera o 294f. **Origem:** item 2.1/2.2 da triagem do relatório do host. Estende `2026-11-25-loop-stop-diagnostics-design.md`.

## Problema

`stall_secs` (600 s) encerra a etapa depois de 600 s sem atividade, e não distingue modelo parado de suíte de testes longa e muda. Run 8502 (`pi`): suíte de ~11 min, encerrada. Run 294f (`opencode`): uma suíte completou em 595,9 s, 4 s abaixo do limite. O modelo chegou a pedir timeout de shell de 900 s. Subir `stall_secs` resolve a suíte, mas faz todo modelo parado custar o mesmo tempo.

## Objetivo

Ferramenta aberta e muda tolera até `tool_stall_secs` de silêncio. Sem ferramenta vista, `stall_secs` continua valendo. Nenhuma etapa morre antes do que morre hoje.

## Abordagens

1. **Dois limites e um sinal de ferramenta, com teto (escolhida).** O sinal pode errar, porque o teto limita o custo.
2. Só subir o padrão de `stall_secs`: simples, mas pune modelo parado em todos os providers.
3. Processo novo ou uso de CPU conta como atividade: sem teto próprio, e um shell esquecido manteria a etapa viva até `stage_timeout_secs`. CPU também falha com teste que espera (`sleep`, rede).
4. Mudar o CLI para emitir progresso: fora do nosso controle no `opencode`.

## Comportamento

### Regra de parada

Com `A` = última atividade (como hoje: byte em `stdout`/`stderr` ou mudança no workspace) e `F` = última vez que o sinal viu ferramenta aberta desde `A`, a etapa é `STALLED` quando:

- `agora − max(A, F) ≥ stall_secs` (limite `stall`); **ou**
- houve `F` e `agora − A ≥ tool_stall_secs` (limite `tool_stall`).

Consequências: com ferramenta aberta, o silêncio total nunca passa de `tool_stall_secs`. Quando a ferramenta some sem evento (processo do `opencode` terminou e o CLI ainda não imprimiu), o modelo ganha de novo `stall_secs` em vez de morrer na hora, sempre dentro do teto. Atividade nova zera `A` e `F`.

`stage_timeout_secs`, `max_duration_secs` e `loop stop` não mudam e vencem sempre: são testados antes, sobre o tempo total.

### Sinal por provider

| Provider | Fonte | Ferramenta aberta quando |
|---|---|---|
| `claude` | eventos | `tool_use` sem o `tool_result` do mesmo id |
| `agy` | eventos | passo `tool` em `ACTIVE` sem mudança de estado |
| `pi` | eventos | `tool_execution_start` sem `tool_execution_end` |
| `opencode` | processos (fallback) | há candidato vivo na árvore da etapa |

- Nos três primeiros vale só o `open_tools` que já existe. Processos **não** entram: lista vazia significa "sem ferramenta", mesmo com shell vivo. Formato de evento desconhecido deixa a lista vazia, ou seja, o comportamento de hoje.
- **Candidato (`opencode`):** processo da árvore, não zumbi, diferente da raiz, iniciado (ticks do kernel) **depois da última fronteira de passo**. Fronteira = linha `step_start` ou `step_finish`, com o instante da leitura do bloco de `stdout`. Antes da primeira fronteira não há candidato.
  - Por que não "depois da última saída" (usado no diagnóstico): em ferramentas paralelas, `A` termina e imprime `tool_use` enquanto `B`, mais antiga que essa saída, ainda roda. `B` seria perdida.
  - Shells de passos anteriores ficam de fora sozinhos: a fronteira seguinte é mais nova que eles.
  - Não se usa nome de processo, linha de comando nem CPU.
- O sinal usa a varredura da árvore que já roda a cada 500 ms; não há varredura extra. Falha ao ler `/proc` significa "sem ferramenta".

### Quando o sinal erra

| Erro | Efeito | Limite |
|---|---|---|
| Falso positivo: servidor/LSP/shell de fundo iniciado no mesmo passo; evento de fim perdido (`claude`, `agy`, `pi`); ferramenta travada | modelo parado demora mais a ser encerrado | `tool_stall_secs` de silêncio; `STALLED` encerra a run, então o custo extra (`tool_stall_secs − stall_secs`, 600 s no padrão) ocorre no máximo uma vez por run |
| Falso negativo: `opencode` com subagente `task` (sem processo), `step_finish` impresso com ferramenta ainda viva, processo fora da árvore, fronteira lida com atraso | igual a hoje | `stall_secs` |

Um processo de fundo que escreve em `stdout`/`stderr` do CLI ou no workspace já conta como atividade hoje. Isso não muda.

## Configuração

- Chave opcional `tool_stall_secs` no topo do workflow, ao lado de `stall_secs`. Inteiro de 1 a 604800, pelo mesmo `read_positive_u64`.
- **Ausente:** `max(1200, stall_secs)`. Vale para workflows existentes sem edição e respeita quem já subiu `stall_secs`.
- **Presente e menor que `stall_secs`:** erro de validação que cita as duas chaves (sai com uso inválido, como os outros). **Igual:** desliga o recurso.
- Maior que `stage_timeout_secs`: aceito, sem efeito prático (hoje `stall_secs` já é assim).
- **Por que 1200:** passa do maior timeout de shell observado (900 s), para o CLI encerrar a própria ferramenta e contar ao modelo antes do motor; cobre 2× a suíte de 595 s; e cabe no `stage_timeout_secs` padrão (1800 s).
- Sem configuração por etapa, por `models.toml` ou por flag de CLI: nenhum outro limite tem.
- Os 4 presets e o TOML do `loop new` ganham `tool_stall_secs = 1200`. Uma versão antiga do `tchelvis` recusa o arquivo com "chave desconhecida" (aceito: o erro é claro). Preflight, free check e teste do sandbox não usam o limite.

**Limite conhecido:** com 1800 s de `stage_timeout_secs`, duas suítes de ~11 min mais o modelo podem terminar em `STAGE_TIMEOUT`. Projetos assim precisam subir esse valor (e `max_duration_secs`) no workflow.

## Estado e relatório (tudo aditivo e opcional)

- `Execution.stop` ganha `limit` (`"stall"` ou `"tool_stall"`, só em `STALLED`) e `tool_signal` (`source`: `"events"` ou `"processes"`; `names`: até 10, só nomes curtos, pelos mesmos filtros de `sanitize_stop`), presente quando o sinal via ferramenta no encerramento.
- `Execution.tool_silence` (`longest_ms`, `source`): gravado em **qualquer** desfecho quando um silêncio passou de `stall_secs` e foi tolerado. É a prova, no host, de que o limite novo salvou a etapa.
- `schema_version` não muda. `state.json` antigo continua legível; sem os dados, as chaves não aparecem.
- Motivo: `STALLED` continua no texto. No limite novo vira `etapa travada sem atividade com ferramenta aberta (STALLED, tool_stall_secs): <dica>`; no antigo, o texto de hoje.
- Dica: com `tool_signal` de processos, `possível ferramenta em execução: <nomes> (processos iniciados no passo atual)`. O resto da ordem não muda.
- `report.md`, em `## Etapas interrompidas`: `- **Limite aplicado:** tool_stall_secs (1200 s); ferramenta vista por processos do passo atual` (ou `eventos do CLI`). Quando há `tool_silence`, seção `## Silêncio longo de ferramenta` com uma linha por execução: etapa, duração e fonte.

A frase da spec anterior "nenhuma decisão do motor passa a depender do diagnóstico" segue válida: a decisão usa o sinal ao vivo, não o bloco `stop`, que só é colhido depois.

## Desenho

- `workflow.rs`: campo `tool_stall_secs` em `UnresolvedWorkflow` e `Workflow` (repassado em `parse_workflow` e em `models.rs`), constante padrão e validação.
- `adapters.rs`: `EventParser::tool_view()` sem consumir o parser: `Open(nomes)`, `NoneOpen` ou `Unseen { step }` (`opencode`; `step` conta fronteiras).
- `process.rs`:
  - `StallTracker` puro (sem I/O nem relógio próprio; recebe `now_ms`): `activity`, `tool_seen`, `verdict`.
  - `tool_candidates` pura sobre `ProcStat` da árvore, raiz e ticks da fronteira.
  - `run_process_diagnosed` recebe o limite e o leitor do sinal; `run_process` e `Limits` mantêm a assinatura e ficam com o recurso desligado.
- `engine.rs`: liga parser e monitor (o parser é usado por duas closures: `RefCell`), grava os campos. `state.rs` e `report.rs`: campos e texto.

**Duas entregas, nesta ordem:** **(A)** regra, configuração e sinal por eventos (`claude`, `agy`, `pi`), com causa confirmada no run 8502; **(B)** fallback do `opencode`. A não depende do resultado do 294f.

## O que muda com o resultado do 294f

| `## Etapas interrompidas` mostra | Conclusão |
|---|---|
| processo da suíte (`run_tests.sh`, Godot) iniciado depois da última saída | causa provada; A e B como estão |
| processo da suíte, mas **antigo** em relação à última saída | pode ser ferramenta paralela (a fronteira cobre) ou resto de ferramenta anterior; conferir idade contra o último `step_start` do log antes de fazer B |
| só CLI, nenhum processo novo, `step_start` | stream travado sem ferramenta (ou subagente `task`, que não tem processo): A segue (`pi`); **B não corrige o 294f** e fica suspensa; o caminho passa a ser repetir a etapa (outra spec) |
| CLI morto, ou lista de processos vazia | problema de coleta (por exemplo no `bwrap`, não validado): corrigir antes de B |
| `STAGE_TIMEOUT` ou `PASS` | nada provado; B só com a evidência dos 595,9 s |

## Critérios de aceitação e testes

Regra e candidatos são testados nas funções puras, com tempo passado por parâmetro. Testes de ponta a ponta usam CLI falso, relógio injetado e arquivos de prontidão, sem rede e sem `sleep` como sincronização.

1. Sem ferramenta, `STALLED` com silêncio ≥ `stall`, nunca com menos (regressão: testes atuais passam sem mudança de comportamento).
2. `StallTracker`, `stall` 600 e `tool_stall` 1200: ferramenta vista até o fim vive em 600 e 1199 s e morre em 1200 com limite `tool_stall`; some em 700 → morre em 1200 pelo teto (não em 1300); some em 300 → morre em 900 com `stall`; atividade em 700 reinicia tudo; `tool_stall == stall` é idêntico ao comportamento de hoje.
3. `pi`, `claude` e `agy` falsos, com ferramenta aberta e silêncio entre os dois limites, terminam `Ok` (relógio injetado) e gravam `tool_silence` com `events`; com a ferramenta fechada e um shell de fundo vivo, `STALLED` em `stall`.
4. Ferramenta aberta para sempre: `STALLED`, `limit = tool_stall`, árvore encerrada (inclusive `setsid`).
5. `stage_timeout` menor que `tool_stall` → `TIMEOUT`; `max_duration` restante menor → `MAX_DURATION`; `loop stop` durante a espera → `CANCELLED`.
6. (B) `tool_candidates`: raiz, zumbi, processo anterior à fronteira ou no mesmo tick, e ausência de fronteira não são candidatos; processo posterior é.
7. (B) `opencode` falso: filho novo e mudo depois de `step_start` sobrevive a `stall`; shell do passo anterior vivo depois de novo `step_start` não; ferramentas paralelas (`tool_use` de uma, a outra aberta) sobrevivem. O CLI falso só abre o filho depois de ver a fronteira no log da etapa e passar um tick do kernel (10 ms), porque o início do processo é real mesmo com relógio injetado.
8. `tool_view` em fixtures: aberta, fechada, paralelas com o mesmo nome, linhas inválidas, campos renomeados → `NoneOpen` (`opencode`: `step` conta só `step_start`/`step_finish`).
9. Workflow: ausente → `max(1200, stall_secs)`; `0`, não inteiro, acima do teto e menor que `stall_secs` → erro citando `tool_stall_secs`; presets e `loop new` validam.
10. `state.json` sem os campos carrega; ida e volta dos campos; segredo em nome de ferramenta ou processo não chega a `state.json` nem a `report.md`; run sem silêncio longo gera o mesmo `report.md` de hoje.
11. Host, dentro do `bwrap` (não automatizado): suíte muda de mais de 600 s com `pi` e, em B, com `opencode`, termina a etapa e mostra `## Silêncio longo de ferramenta`.

## Não objetivos

Reiniciar ou repetir a etapa; `resume`; encerrar só a ferramenta travada; limite por ferramenta, etapa ou provider; mudar `stall_secs`, `stage_timeout_secs` ou `max_duration_secs`; ler o timeout que o modelo pediu; detectar subagente do `opencode`; analisar CPU ou linha de comando; mudar o que conta como atividade.

## Decisões do usuário (2026-11-25: aprovadas as recomendações)

1. **Padrão 1200 s ligado para todos os workflows?** Recomendado: sim. Alternativas: 1500 s (sobra menos dos 1800 s da etapa) ou só quando a chave existir (o problema continua para quem não editar).
2. **Fazer A já e B depois do 294f?** Recomendado: sim. Se o 294f não chegar, fazer B pela evidência dos 595,9 s, sabendo que ela não prova a causa.
3. **Subir o `stage_timeout_secs` padrão (1800 s)?** Recomendado: não aqui; ajustar no workflow do projeto Godot.
