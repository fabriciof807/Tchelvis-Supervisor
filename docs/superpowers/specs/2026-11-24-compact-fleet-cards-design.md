# Cards compactos da Fleet — design

Data: 2026-11-24 · Status: aprovado pelo usuário · Tamanho: pequeno (só o mod)

**Intenção:** cards com cerca de metade da altura; do agente, só o ícone; o ID do terminal sai do card.

**Abordagens:** duas linhas (**escolhida**); uma linha (sem espaço para branch e resumo); ID no tooltip (sem confirmação de que o `gpui-kit` suporta).

**Design**
- Linha 1: estado, ícone, projeto, status. Linha 2: branch, resumo e selo de PR ou issue.
- Saem o texto padrão do supervisor e os selos `Local`, `Exact` e `Inferred`; `Ambiguous` fica.
- Pedido de permissão, filho bloqueado e filhos ativos: linha própria, só quando existem.
- O ID fica no título do Inspector e, no card, só quando dois cards seriam iguais.
- Agente sem ícone conhecido mantém o nome curto. Loop mostra `loop-<RUN_ID>` (cortado, mantém o fim).
- O texto não se corta sozinho: os limites dependem do status, do recuo de filho e do selo.
- Clique, filtros e ordem não mudam.
