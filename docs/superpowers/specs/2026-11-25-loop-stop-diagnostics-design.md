# Loop: diagnóstico de etapa parada (`STALLED` e `TIMEOUT`)

**Data:** 2026-11-25. **Origem:** relatório do host (10 runs, 5 `STALLED` do `opencode` e 1 do `pi`), item 1 da triagem, aprovado pelo usuário.

## Problema

O log não separa "modelo sem resposta" de "ferramenta longa". Medido no ai-box:

- `opencode run --format json` 2.3.0: nada é impresso enquanto a ferramenta roda (`tool_use` só sai ao terminar) nem enquanto o modelo escreve. O último evento é `step_start` nos dois casos.
- `pi --mode json` 1.5.2: `tool_execution_start`, um `tool_execution_update` vazio e depois silêncio até a ferramenta imprimir.
- Depois de encerrar a etapa, o motor mostra "pi terminou sem evento 'agent_settled'", que é efeito do encerramento.

## Comportamento

1. Em `STALLED`, `TIMEOUT` e `MAX_DURATION` por etapa, a execução grava em `state.json` um bloco `stop` (opcional, aditivo). Ele é colhido **antes** de encerrar a árvore de processos:
   - último evento (tipo e, se for de ferramenta, o nome) e o instante dele;
   - tempo desde a última saída em `stdout` (`silent_ms`);
   - se o CLI ainda estava vivo;
   - ferramentas em execução segundo o CLI. `claude`, `agy` e `pi` informam. O `opencode` não informa (`null`);
   - até 20 processos da árvore: nome curto (`comm`), idade, CPU usada e se começou depois da última saída; mais o total.
2. **Nunca** gravar linha de comando, argumentos, ambiente ou caminho de processo: podem ter segredos. O `log` da etapa já tem o comando.
3. Uma dica de uma linha entra no fim do motivo (linha `RESUMO` e `report.md`):
   - ferramenta aberta: `ferramenta em execução: <nomes>`;
   - senão, processo iniciado depois da última saída: `processos iniciados depois da última saída: <nomes> (possível ferramenta em execução)`;
   - senão, nenhum evento recebido: `o CLI não emitiu nenhum evento`;
   - senão: `nenhuma ferramenta detectada em execução (provável espera pelo modelo)`.
   O texto é um indício, não uma certeza, e as palavras "possível" e "provável" são obrigatórias.
4. Em etapa encerrada pelo motor, o erro de "terminou sem evento final" não é mostrado. Um erro explícito do fluxo (por exemplo `auto_retry_end` sem sucesso) continua.
5. `report.md` ganha a seção `## Etapas interrompidas` quando houver algum `stop`.

## Fora do escopo

`tool_stall_secs`, reinício da etapa, `resume`, mudança de qualquer limite. Nenhuma decisão do motor passa a depender do diagnóstico. Falha ao colher o diagnóstico nunca muda o resultado da etapa.
