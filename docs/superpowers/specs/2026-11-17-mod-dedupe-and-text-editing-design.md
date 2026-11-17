# Mod: seletor único e edição de texto com cursor — design curto

Data: 2026-11-17 · Status: aprovado pelo usuário

## Intenção

1. O seletor Home / Inspector / Doctor / Chat aparece em três lugares (barra do topo, cabeçalho da
   gaveta, links da barra de status). Fica só o do topo.
2. Os campos de texto do mod só acrescentam no fim e apagam o último caractere: setas, Home, End e
   Delete não funcionam (relatado no chat). Todos os campos ganham um cursor real.

## Decisões

- **Seletor:** remover a fileira de abas do cabeçalho da gaveta (`renderInspector`, botões
  `🏠 Home`/`🔍 Inspector`/`🩺 Doctor`/`💬 Chat`), mantendo `↻` e `×`; remover da barra de status os
  links Home · Inspector · Doctor · Chat e seus separadores, mantendo `Uso` e
  `CTRL-SHIFT-P commands`. O seletor do topo (`renderPill`, `ck-pill-view-*`) não muda.
- **Edição:** uma função pura compartilhada por todos os campos (`task`, `chat`, `fallbackHost`,
  `modelFilter`, `command`, `prompt`, `label`): `←`/`→` movem um caractere; `Home`/`End` vão ao início
  e ao fim da **linha** atual (campos multilinha: `task`, `chat`) ou do texto; `Delete` apaga à
  direita; backspace apaga à esquerda do cursor; caracteres e o `\n` do Shift+Enter entram na posição
  do cursor. O cursor `▏` é desenhado na posição real. Ao focar um campo, ou quando o texto muda por
  fora (enviado, limpo), o cursor vai para o fim.
- Unidade do cursor: caracteres Unicode (code points), para acentos e emoji não serem cortados ao meio.

## Abordagens

- **Função pura + tabela de campos (escolhida):** um único caminho de teclado, testável em Node.
- Corrigir só o chat: deixaria seis campos com o mesmo defeito. Descartada.
- Campo de texto nativo do GPUI: a API de mods não expõe um. Descartada.

## Fora do escopo

Seleção de texto, copiar/colar, Ctrl+←/→ por palavra, ↑/↓ entre linhas. O Files com yazi tem design
próprio (aguarda o `AGENTS.md` do Herdr).
