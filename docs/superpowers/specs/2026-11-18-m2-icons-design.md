# M2 — Ícones no lugar dos emojis do mod — design

Data: 2026-11-18 · Status: aprovado pelo usuário

## Intenção

Os emojis coloridos do mod mudam de aparência conforme a fonte e o tema e destoam dos ícones do
Herdr. Trocar os 16 emojis por ícones SVG monocromáticos, tingidos pelo tema. Abre a trilha de
interface (M3 layout compacto, M4 Inspector multi-terminal).

## Escopo (decisão do usuário)

- **Só os emojis** (16): 🏠 🔍 🧠 🩺 💬 📋 📁 🚀 ⚡ ⚠ 🔑 🖥 ⚙ ⏱ ✨ 🐟.
- **Fora:** símbolos tipográficos (● ○ ◌ ↻ ↗ → ↳ ↵ ▾ ▸ ▶ ◁ ▷ ✓ ✎ ⎇ ⌘ ⏻ ✦ ◎ ✧ ◈ ◆) continuam como
  texto; a TUI e o Rust não têm emojis e não mudam.
- O `⚠` que o mod procura na saída do supervisor para destacar alertas (detecção, não exibição)
  continua, escrito como `"⚠"`.

## Abordagens

1. **Arquivos SVG em `mods/tchelvis/icons/` — escolhida.** Mesmo mecanismo do Herdr
   (`svg("mods/workspace-chrome/spinner.svg")`), comprovado. O `tchelvis setup` instala os SVGs
   embutidos no binário, como já faz com `main.js`; o `doctor` acusa ícone faltando ou diferente.
2. SVG embutido no `main.js`: um arquivo só, mas não se sabe se `svg()` aceita conteúdo em vez de
   caminho. Descartada por ora.

## Design

- **Fonte:** Lucide (licença ISC), versão fixa; copiar só os 16 ícones e a licença
  (`icons/LICENSE-lucide.txt`).
- **Mapa:** 🏠 `house`, 🔍 `search`, 🧠 `brain`, 🩺 `stethoscope`, 💬 `message-square`,
  📋 `clipboard-list`, 📁 `folder`, 🚀 `rocket`, ⚡ `zap`, ⚠ `triangle-alert`, 🔑 `key-round`,
  🖥 `monitor`, ⚙ `settings`, ⏱ `timer`, ✨ `sparkles` (Antigravity), 🐟 `fish` (DeepSeek).
- **Mod:** um mapa único nome → caminho; `icon(nome, tamanho, cor)` (cor e tamanho do tema/contexto);
  `iconLabel(...)` para ícone + texto em linha. Textos que hoje embutem o emoji (`"⚡ Poll Now"`,
  `` `🧠 ${modelo}` ``, `"⚠ 1 filho"`, títulos de seção) viram ícone + texto; funções puras que
  devolvem texto perdem o emoji (o ícone é posto por quem desenha).
- **Instalação:** `tchelvis setup` grava `~/.config/herdr-app/mods/tchelvis/icons/*`; a verificação do
  setup e o `doctor` (`herdr_mod`) passam a conferir os ícones; `uninstall` remove pelo manifesto.

## Validação

Testes: nenhum emoji no `main.js`; todo ícone do mapa existe em `icons/`; o setup instala todos os
arquivos da pasta. Host: `herdr-app check ~/.config/herdr-app/mods/tchelvis` e olhar nos temas claro
e escuro (`herdr-app shot`).
