# Chat com o modelo do supervisor — design

Data: 2026-11-17 · Status: aguardando aprovação

## Intenção

Um chat de bolso no painel para perguntas básicas sobre o que o tchelvis já sabe: quais agentes
estão ativos e o que estão fazendo, e o que entrou nas Tarefas do dia da Home. Usa o mesmo modelo
do supervisor (container `llama.cpp`, ou o fallback por API quando ativo), sem subir outro modelo.

Exemplos: "quais agentes estão esperando por mim?", "o que o t11g está fazendo?", "quais tarefas
do projeto X ainda estão abertas?", "resuma o que entrou hoje", "o que eu deveria fazer primeiro?".

## Objetivos

- Responder perguntas sobre os agentes ativos e as Tarefas do dia usando o modelo efetivo do
  supervisor.
- O contexto é montado pelo tchelvis e enviado junto com a pergunta; o usuário não cola nada.
- Funciona na gaveta do Herdr Desktop (aba nova) e no terminal (`tchelvis chat ask`).

## Não-objetivos

- **Agir** (fase B, decidida para depois): criar, marcar ou editar tarefas; responder a agentes;
  qualquer `hc send`. O chat só lê e responde.
- Ferramentas/MCP ou o modelo escolhendo o que consultar (modelo de 350M–1.7B erra nisso).
- Enviar as telas dos terminais (não cabem em 4096 tokens com vários agentes).
- Histórico persistente entre reinícios do app; REPL interativo no terminal.

## Anti-objetivos (falha mesmo que funcione)

- O chat atrasar de forma perceptível as decisões do supervisor.
- O chat ligar o modelo sozinho ou impedir o zero-idle de desligá-lo.
- O modelo inventar estado ("o agente X terminou") que não está no contexto, sem dizer que não sabe.
- A resposta parecer uma ação executada.

## Abordagens consideradas

1. **Subcomando Rust `tchelvis chat ask` + aba "Chat" na gaveta do mod — escolhida.** Reaproveita a
   escolha de backend (container / fallback por API / Ollama legado), a chave e os testes; o mod só
   desenha a conversa. Mesmo padrão de processo-por-chamada do `supervisor tick`.
2. Mod chamando o `llama-server` por HTTP direto: duplicaria a escolha de backend em JS e ignoraria
   fallback e chave. Descartada.
3. Só `tchelvis chat` no terminal: o usuário usa o Herdr Desktop; vem incluído na 1.
4. Interface web embutida do `llama-server` (`http://127.0.0.1:11435`, com MCP desde 2026-03): não
   conhece agentes nem tarefas sem colar contexto ou depender do modelo pequeno chamar ferramentas,
   e fica fora do Herdr. Pesquisados também Pi Durable, pi-supervisor, DeepSeek Super-Agent e o
   próprio Herdr: nenhum faz isto.

## Design

### CLI — `tchelvis chat ask`

- `tchelvis chat ask "<pergunta>"`: uma pergunta, resposta em texto no stdout.
- `tchelvis chat ask --json [--request '<json>']`: lê do argumento `--request` (o mod não manda stdin) ou do stdin `{"question": "...", "history": [{"role": "user"|"assistant", "content": "..."}]}`
  e escreve `{"answer": "...", "model": "<rótulo>", "via_fallback": bool, "context_chars": N}` ou
  `{"error": "...", "code": "llm_stopped"|"llm_error"|"bad_input"}` (código de saída ≠ 0 no erro).
- O histórico vem de quem chama (o mod); o CLI não guarda nada em disco.

### Contexto enviado ao modelo

Montado por uma função pura (testável) a partir de dados já existentes:

- **Agentes** (`hc` via `HcBackend::list_sessions` + `supervisor_state.json`): por sessão, nome,
  endereço, programa (`claude`, `pi`…), estado hc (`working`/`blocked`/…), `kind`/`msg` do pedido
  pendente, pasta (só o nome final), `rolling_summary`, motivo do card pendente e a última
  auto-resposta. Bloqueados primeiro.
- **Tarefas** (`~/.local/share/tchelvis/daily_tasks.json`): abertas primeiro, depois as concluídas
  hoje; título (texto inteiro, até um limite por tarefa), etiqueta (`Geral` quando sem projeto),
  prioridade, data de criação.
- Data/hora atual.
- **Orçamento**: limite de caracteres para o bloco de contexto (ponto de partida ~6000, ≈1,5–2k
  tokens) com corte explícito ("… e mais N agentes/tarefas"); histórico limitado às últimas 6
  trocas. Resposta com `max_tokens` ~512. Tudo dentro do `ctx_size` 4096 do container.

### Prompt

Prompt de sistema em português: responder só com base no contexto; dizer quando a informação não
está lá; nunca afirmar que executou algo (o chat não age); respostas curtas. Sem JSON schema —
texto livre (diferente da avaliação do supervisor).

### Backend

- Usa o modelo **efetivo** do supervisor: fallback ativo → endpoint do fallback (mesma chave e
  `RedactingBackend` para erros); senão o container `llama.cpp`; com `[llm_container] mode = off`, o
  caminho Ollama legado. Requer um método novo de completação em texto livre ao lado de `evaluate`.
- **Nunca liga o modelo.** Container parado e sem fallback ativo → erro `llm_stopped`; o mod mostra
  o botão `▶ Load LLM` existente. O chat não conta como sessão ativa para o zero-idle.
- Concorrência: o container roda `-np 1`, então um pedido do chat e uma avaliação do supervisor
  entram na fila do `llama-server`. Aceito: no pior caso uma decisão atrasa o tempo de uma resposta
  do chat. A regra fixa de leitura não usa a LLM e não é afetada (`530ba7b`).
- Timeout do pedido: 120 s.

### Mod — aba "Chat" na gaveta direita

- Quarta aba na gaveta (`Home` / `Inspector` / `Doctor` / `Chat`), aberta pelo seletor da gaveta.
- Cabeçalho: rótulo do modelo (`LFM3-1.7B…`, com `(Fallback · <host>)` em tom de aviso quando a
  pergunta vai para a API — o contexto sai da máquina nesse caso).
- Lista de mensagens (usuário/modelo), campo de texto que cresce (Enter envia, Shift+Enter quebra
  linha — mesmo comportamento do campo das Tarefas do dia), botão `Limpar`.
- Enquanto espera: `◌ Pensando…` e campo desabilitado (uma pergunta por vez).
- Erro `llm_stopped`: mensagem "LLM desligada" + `▶ Load LLM`. Outros erros: texto do erro na
  conversa, em tom de erro.
- Histórico em memória do mod (shared state), apagado ao reiniciar o app ou em `Limpar`.
- Respostas mostradas como texto (sem botões de ação), para nunca parecerem uma ação executada.

### Testes

- Rust: montagem do contexto (ordem, cortes de orçamento, tarefa sem projeto → `Geral`, nada de
  tela de terminal), corpo do pedido (sem schema, `max_tokens`, histórico cortado), roteamento
  fallback/container/parado com backends falsos, formato JSON de saída e erro.
- Mod (Node): funções puras da aba (montar o histórico enviado, estado de espera, render do erro).
- Validação no host: perguntas reais com o 1.7B e com o 350M; tempo de resposta; checar que uma
  decisão do supervisor durante o chat ainda sai.

## Deixado para quem constrói

- Nome e local do módulo novo (ex.: `src/supervisor/chat.rs`) e do método de completação livre no
  trait ou ao lado dele.
- Formato textual exato do bloco de contexto e o número fino do orçamento.
- Detalhes visuais da aba, seguindo o estilo das abas existentes.

## Decisões minhas (para conferir)

- Histórico só em memória; nada de conversa gravada em disco.
- Com fallback ativo, o contexto (resumos dos agentes, títulos das tarefas) vai para a API do
  fallback, com aviso visível na aba.
- Erro em vez de carregar o modelo quando ele está parado.
- Sem telas de terminal no contexto; só os resumos que o supervisor já mantém.
