# Regras configuráveis do supervisor (`[guardrails]`) — design

**Data:** 2026-11-22, revisado em 2026-11-23 · **Status:** abordagem A escolhida pelo usuário; aprovado em 2026-11-23 · **Tamanho:** projeto (M10)

## Intenção

As regras do que o supervisor aprova ou bloqueia são fixas em [`guardrails.rs`](../../../src/supervisor/guardrails.rs). O usuário só escolhe o modo e `max_auto_replies`. O M10 adiciona a seção opcional `[guardrails]` ao `~/.config/tchelvis/supervisor.toml` para:

- aprovar, por regra fixa e sem LLM, comandos do fluxo do usuário (`just test`, `cargo test *`);
- proibir comandos e caminhos, mesmo os que as listas embutidas aprovam;
- mostrar antes e depois qual regra decidiu.

## Objetivos e não objetivos

**Objetivos:** previsível (mesma tela, mesma decisão, com ou sem LLM); erro de configuração nunca aumenta o que é aprovado; efeito visível na CLI, no Doctor e no Log.

**Não objetivos:** ser sandbox (um comando aprovado executa o que quiser); substituir as permissões do agente; afrouxar o piso.

**Fora desta fase:** `llm_instructions` (a LLM não decide pedidos de permissão); regras por diretório ou agente; regex; liberar pipelines; edição visual no mod.

## Fatos do código que guiam o design

- `sanitize_auto_reply` recusa **toda** resposta da LLM em pedido de permissão (`kind=permission`, menu `1. Yes` ou `msg` nomeando ferramenta). No setup do usuário só o Claude pede permissão, então só o fast-path aprova algo.
- O app executa `tchelvis supervisor tick` a cada 12 s, um processo novo por vez: a configuração é relida sozinha e não existe "último valor válido" em memória.
- `SupervisorConfig::load()` volta aos **padrões do arquivo inteiro** em erro de TOML. Para regras `deny_*`, isso liberaria o que estava proibido.

**Pré-requisito:** lote M (endurecimento do fast-path: `&`, `rg --pre`, `git --output`, expansões do shell, descrição lida como comando).

## Abordagens

| | Como | Troca |
|---|---|---|
| **A (escolhida)** | Uma lista `allow_commands`, regra fixa, vale também no menu do Claude | Simples e testável; cada entrada aprova de verdade |
| B | `allow_fast_path` + `allow_commands` (só quando a LLM responde) | A segunda lista só valeria em `[y/N]` simples |
| C | Como B, com a LLM aprovando no menu do Claude | Remove uma proteção criada após os testes no host |

## Ordem de decisão

1. **Piso fixo, imutável:** `FORBIDDEN_TOKENS`, `curl|sh`, `wget|sh`, `> /`, metacaracteres e expansões do lote M, opções perigosas de `rg`/`git`, caminhos sensíveis embutidos, `kind=question`, só teclas simples, nunca as opções "always allow/auto mode". Nada abaixo reverte isto.
2. **`deny_commands` / `deny_paths`:** vencem qualquer liberação.
3. **Regra fixa:** leitura embutida, depois `allow_commands` e `allow_tools`.
4. **LLM:** como hoje, sujeita a 1 e 2 e a `llm_auto_reply`.

O modo continua valendo: `AlertOnly` e `Paused` nunca enviam resposta. `max_auto_replies` também vale para `allow_commands`.

## Schema

```toml
[guardrails]
# Comando exato. "*" no fim aceita argumentos adicionais.
allow_commands = ["just test", "cargo test *", "uv run pytest *", "git branch"]

# Vence tudo acima. Procurado em qualquer ponto do pedido pendente.
deny_commands = ["cargo publish", "npm publish", "docker rm", "git commit"]

# Comparado por segmento do caminho, sem diferenciar maiúsculas.
deny_paths = [".kube", ".aws", "prod/secrets"]

# Subconjunto de: Read, Glob, Grep, LS. Padrão: ["Read"]. [] desliga.
allow_tools = ["Read"]

builtin_read_only  = true   # ls, cat, rg, git status...
builtin_build_test = true   # cargo test, pytest... (só em [y/N] respondido pela LLM, como hoje)
llm_auto_reply     = true   # false: a LLM só resume e alerta
```

Sem a seção, o comportamento é o de hoje.

## Semântica

### `allow_commands`
- Entrada e comando são separados com `shlex` e comparados token a token. Sem `*`, precisam ser iguais: `git branch` **não** aprova `git branch -D main`.
- `*` só como último token isolado; aceita zero ou mais tokens, e o comando inteiro ainda passa pelo piso e pelos `deny_*`.
- Só comando único: piso de metacaracteres, expansões e texto truncado do lote M vale igual.
- Sem correspondência difusa: `cargo  test` (espaços) casa; `/usr/bin/cargo test`, `FOO=1 cargo test` e `env cargo test` não casam.
- Vale nas mesmas origens do fast-path atual: `msg` do hc, prompt em linha e tela. Na tela sem `msg`, exige o cabeçalho `Bash command` com uma única linha de comando, ou o prompt em linha; a heurística da "linha mais próxima" sem cabeçalho continua só para a leitura embutida.

### `deny_commands`
- O `msg`, o comando identificado e todas as linhas da área pendente são divididos em tokens (espaços, aspas e `; | & ( )` separam). A entrada casa se seus tokens aparecem em sequência em qualquer ponto: pega `cd x && cargo publish` e `bash -c "cargo publish"`.
- Não aceita `*`. Sem diferenciar maiúsculas.
- Falso positivo só gera alerta. Limite conhecido: `npm --silent publish` não casa com `npm publish`.

### `deny_paths`
- Cada token é dividido por `/`, `=` e `:`; a entrada casa se seus segmentos aparecem em sequência. `prod` casa com `./prod/x`, não com `reprod/x`.
- Vale para comandos e para alvos de `allow_tools`. Limite conhecido: globs e caminhos montados em tempo de execução.
- Além disso, o piso passa a recusar glob em segmento iniciado por ponto (`cat .e*`), que hoje contorna a lista sensível.

### `allow_tools`
- Lista fechada: `Read`, `Glob`, `Grep`, `LS`, nome exato. `Bash`, ferramentas de escrita e MCP são recusadas na carga.
- Alvo único e sem espaços, sob as checagens de caminho. Só `Read` tem formato real capturado; `Glob`, `Grep` e `LS` exigem captura do host antes de serem implementadas e, até lá, são recusadas com aviso.

### Efeito de uma proibição
O fast-path não aprova. Se a LLM propuser resposta afirmativa, vira `AlertOperator` `Normal`, sem payload, com motivo `[Resposta automática bloqueada: regra deny_commands "cargo publish"]`. Se também estiver no piso destrutivo, continua `Critical` com payload `n`. Respostas negativas (`n`) são mantidas.

## Carga e erros (nunca abre)

| Situação | Efeito |
|---|---|
| Arquivo ausente ou sem `[guardrails]` | Comportamento atual |
| Entrada de `allow_*` inválida | Ignorada, com aviso; o resto vale |
| Entrada de `deny_*` inválida (vazia, aspas abertas, `*`) | `[guardrails]` inválido |
| Chave desconhecida ou tipo errado em `[guardrails]` | `[guardrails]` inválido |
| TOML ilegível ou arquivo sem permissão de leitura | `[guardrails]` inválido |

**`[guardrails]` inválido desliga toda resposta automática** (fixa e LLM); propostas viram alerta, com motivo visível. O restante do arquivo, se legível, continua valendo: `[guardrails]` é lido à parte e não derruba modelo, fallback ou container.

Entradas de `allow_commands` inválidas: vazias; aspas abertas; `*` fora do fim ou sozinho; contidas no piso (token proibido, metacaractere, caminho sensível); cobertas por um `deny_*`; ou shell, interpretador ou executor seguido direto de `*` (`bash`, `sh`, `zsh`, `fish`, `env`, `xargs`, `eval`, `exec`, `sudo`, `ssh`, `nohup`, `timeout`, `time`, `python*`, `node`, `bun`, `deno`, `perl`, `ruby`, `npx`, `uvx`). `python scripts/check.py` exato é aceito.

`supervisor watch` relê `[guardrails]` a cada tick. `tick` já relê por ser um processo novo.

## CLI e observabilidade

- **`tchelvis supervisor rules [--json]`:** regras efetivas, origem (piso, embutida, usuário), avisos e estado (`ok`, `invalid`).
- **`tchelvis supervisor rules test "<comando>" [--tool <nome>] [--json]`:** decisão e `rule_id`, sem enviar nada. Usa a mesma função do supervisor. Saída 0 para aprovado, 1 para não aprovado, 2 para erro de uso.
- **`rules test --screen <arquivo> [--msg <texto>] [--kind <kind>]`:** o mesmo com uma tela real.
- **`rule_id`:** `floor:<motivo>`, `deny_command:<entrada>`, `deny_path:<entrada>`, `builtin_read_only`, `builtin_tool:Read`, `allow_command:<entrada>`, `allow_tool:<nome>`, `requires_llm`, `llm_auto_reply_off`, `guardrails_invalid`.
- **Doctor:** estado e avisos de `[guardrails]`; inválido aparece como erro.
- **`supervisor_events.jsonl`:** campo opcional `rule_id` em `auto_reply` e `auto_reply_blocked`; linhas antigas continuam legíveis. O detalhe da aba Log mostra a regra.
- **Textos:** "Comando somente leitura aprovado pela regra fixa" passa a "Comando aprovado pela regra `<rule_id>`" quando a origem é do usuário.

## Organização

- `src/supervisor/rules.rs` (novo): tipos, validação e casamento, sem I/O nem relógio.
- `guardrails.rs`: fast-path e `sanitize_auto_reply` recebem `&GuardrailRules` e devolvem o `rule_id`. Uma única função decide; `rules test` não duplica lógica.
- `config.rs`: lê `[guardrails]` à parte.
- `watcher.rs`, `events.rs`, `doctor.rs`, `main.rs` e a aba Log do mod: integração.

## Critérios de aceite

1. Sem `[guardrails]`, todos os testes atuais passam sem mudança de comportamento.
2. Com `allow_commands = ["just test"]`, o menu do Claude com `msg` `approve Bash: just test` recebe `1` com a LLM parada; `just test x` e `just test & evil` não.
3. Uma proibição vence leitura embutida, `allow_commands` e LLM.
4. Nenhuma linha da tabela de erros aprova mais do que aprovaria sem ela.
5. Para cada comando dos testes, `rules test` e o supervisor dão o mesmo resultado e `rule_id`.
6. `[guardrails]` inválido não altera modelo, fallback nem container.
7. No host: um comando liberado e um proibido, pedidos por um Claude real, aparecem no Log com a regra.

## Minhas escolhas para você conferir

- `[guardrails]` inválido desliga as respostas automáticas, em vez de usar os padrões.
- Chave desconhecida é erro: um `deny_command` digitado errado não pode passar despercebido.
- `builtin_build_test` não vira fast-path: `cargo test` no Claude só é aprovado se você o listar.
- `llm_instructions` e os antigos `disable_*` saíram.

## Deixado para quem implementar

Nomes internos, formato da saída humana de `rules` e detalhe visual na aba Log.
