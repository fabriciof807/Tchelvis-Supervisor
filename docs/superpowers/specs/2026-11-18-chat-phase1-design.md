# Chat fase 1 — campos nativos e conversa usável no dia a dia — design

Data: 2026-11-18 · Status: aprovado pelo usuário (fase 1 da pesquisa `.work/returns/chat-ux-research.md`)

## Intenção

O chat funciona mas não está bom de usar todo dia. O que mais incomoda (usuário): o **campo de
texto** — o cursor `▏` é inserido como caractere e separa o texto (`hel▏ lo`) — e a conversa **não
rola até o fim** quando uma mensagem é enviada. A edição manual feita em 2026-11-17 (`applyTextKey`)
partiu de uma premissa errada: a API de mods **tem** campos nativos.

## Fatos da API (gpui-kit.d.ts do host do usuário, 2026-11-18)

- `InputState.new({placeholder?, value?})` / `Input.new(state)` (uma linha);
  `TextareaState.new({placeholder?, value?, rows?})` / `Textarea.new(state)` (várias linhas, com
  `set_rows`, `set_auto_grow(min,max)`, `set_soft_wrap`). Ambos: `value()`, `set_value()`, `replace()`,
  `selected_range()`, `set_selected_range()`, `focus()`, `release()`,
  `on("change"|"submit"|"focus"|"blur"|"paste_other", (event, cx) => …)`.
- Estados só podem ser criados no `init` ou em handlers, nunca no `render`. Os controles vêm sem
  estilo: o mod estiliza.
- Guia público (`herdrdev/herdr-app` `defaults/AGENTS.md`): `ScrollHandle` de `gpui-base`
  (`ScrollHandle.new()`, `.track_scroll(h)` num `overflow_y_scrollbar()`, `h.scroll_to_item(i)`),
  `TextView.markdown(id, text)`, animações nativas `.frames([...], ms)`.
- Mods do Herdr que já usam: `defaults/mods/prompt` (Input, `submit`), `defaults/frame/feedback`
  (Textarea).
- Sem atalhos Emacs nos campos nativos (issue #196) — aceito.

## Escopo da fase 1

1. **Campos nativos nos 7 campos**: `chat` e `task` com `TextareaState` (cresce de 1 a 6 linhas);
   `fallbackHost`, `modelFilter`, `command`, `prompt`, `label` com `InputState`. Sai a edição manual
   (`applyTextKey`, `textWithCaret`, `TEXT_FIELDS`, caret `▏`, ramos de digitação do
   `handleKeyDown`). Envio pelo evento `submit` e pelos botões existentes.
2. **Rolagem automática** (`ScrollHandle`) até a última mensagem ao enviar, ao chegar a resposta e
   ao abrir a aba; **foco no campo** ao abrir a aba Chat.
3. **Espera visível**: spinner nativo + segundos decorridos (`◌ Gerando… 6s`).
4. **Markdown** nas respostas (`TextView.markdown`, com texto simples se indisponível).
5. **Perguntas prontas** no chat vazio (4 botões: "Quem está esperando por mim?", "Quais tarefas
   estão abertas?", "Resumo dos agentes", "O que faço primeiro?").
6. **Erro recuperável**: `↻ Tentar de novo` (reenvia a mesma pergunta) e `Editar` (devolve ao campo).
7. **Copiar** resposta (`wl-copy`, senão `xclip`, senão `xsel`), com "Copiado".
8. **`↗ Abrir <agente>`** sob a resposta para cada agente da Fleet citado por endereço ou nome,
   abrindo o terminal pelo caminho já usado na Fleet.

Fora (fase 2): streaming e `■ Parar`. Mantidos: histórico só na memória; limites atuais.

## Abordagens

- **Campos nativos (escolhida):** resolvem cursor, seleção, colar, desfazer e IME de uma vez.
- Consertar o caret manual (desenhá-lo sem ocupar espaço): continuaria sem seleção, colar e IME.

## Riscos e decisões minhas

- Não se sabe se, no `Textarea`, Enter envia (`submit`) ou quebra linha. O chat envia no `submit`
  e mantém o botão `Enviar`; o comportamento exato das teclas é conferido no host.
- `TextView` e `ScrollHandle` são usados via `import * as base from "gpui-base"` com teste de
  existência, para um nome ausente não impedir o mod de carregar.
