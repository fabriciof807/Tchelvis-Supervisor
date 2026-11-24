# Tickets e coordenador de loops no modo orchestrator — design

Data: 2026-11-24 · Status: **aguardando aprovação** · Tamanho: pequeno (instruções e instalação; sem mudança no motor)

## Intenção

Uma spec pronta é dividida em passos claros, que agentes menores e mais baratos concluem com ajuda
do `tchelvis loop`. O coordenador só dispara, junta e reporta, sem carregar código nem diffs no
contexto. O arquivo enviado pelo usuário em 2026-11-24 é inspiração, não contrato. Da skill
`to-tickets` de Matt Pocock (MIT) vêm ideias; o texto é nosso.

## Objetivos, não objetivos e antiobjetivos

**Objetivos:** ticket que um modelo barato termina sem decidir projeto; coordenador que sobrevive à
perda da sessão lendo os tickets; nenhum trabalho juntado sem `PASS`.

**Não objetivos:** novo comando no `tchelvis`; workers do `orch` executando tickets; merge em
branch compartilhada; push; publicar em GitHub, Linear e semelhantes.

**Seria um fracasso mesmo funcionando se:** o coordenador executasse código dos agentes fora do
sandbox; mexesse no checkout do usuário; ou sobrescrevesse uma skill que o usuário já tem.

## Abordagens

| | Como | Troca |
|---|---|---|
| **1 (escolhida)** | Instruções na skill do orchestrator, com `loop run --input-file … --detach` | Pouco código; depende de o modelo seguir as regras |
| 2 (talvez depois) | Loop como executor de uma tarefa do `orch` | Ordem e estado por código; mudança média no que acabou de ser validado |
| 3 (recusada) | Um comando que executa tudo | Faria merge sozinho, o que a spec do loop proíbe |

## 1. Skill `to-tickets`

Skill comum, sem depender do `tchelvis` e sem citar modelos. Recebe uma spec (caminho ou conversa).

1. Lê a spec e o código que ela toca. **Decisão em aberto:** pergunta; nunca inventa.
2. **Cabe em um só loop ou sessão?** Diz que não precisa de tickets e para.
3. Divide em fatias verticais. Cada uma:
   - dá para **demonstrar** sozinha;
   - cabe em uma sessão limpa de um modelo barato;
   - não exige decisão de projeto.
4. Preparação do código vem primeiro. Mudança ampla: criar o novo, migrar em lotes, remover o velho.
5. `blocked_by` só para dependência real. Arquivo em comum, ou dúvida: dependência em vez de
   paralelo. Arquivos que todos tocam (lockfile, changelog, índice de módulos) contam.
6. O último ticket é sempre **integração**: bloqueado por todos, roda a suíte e corrige o conjunto.
7. Mostra a lista (título, bloqueios, o que demonstra, trecho da spec) e o que da spec ficou **sem
   ticket**. Só grava depois da aprovação.
8. Grava um arquivo por ticket em `.loops/tickets/<feature>/NN-<slug>.md` se o projeto tem `.loops/`;
   senão, em `.tickets/<feature>/`. Pasta com tickets: pergunta, não sobrescreve.

```markdown
---
id: "01"
title: Título curto e imperativo
status: ready          # ready | running | merged | failed
blocked_by: []
files: [src/a.rs, tests/a.rs]
workflow:              # opcional: time do loop
run_id:                # preenchido pelo coordenador
---
## Objetivo             (o que passa a funcionar e como demonstrar)
## Da spec              (trecho copiado, com a origem: o loop pode não ter o arquivo da spec)
## Comece por aqui      (arquivos e contratos; se o arquivo mudou de lugar, vale o contrato)
## Fora de escopo
## Critérios de aceite  (cada um falha antes do trabalho)
## Verificação          (comando)
```

## 2. Coordenador, na skill `/orchestration-tchelvis`

Nova seção "Executar tickets com loops". Regras:

- **Contexto:** lê só o cabeçalho dos tickets e as 5 linhas de `loop result`. Não edita código, não
  lê diffs, não lê implementação.
- **Autorização única no início:** mostra a lista, a branch e o limite de loops, e pede para criar
  `integration/<feature>` em um worktree próprio, e juntar ali. Nunca na `main`, sem push. Os loops
  são disparados **de dentro do worktree**, para partirem do `HEAD` da integração.
- **Disparo:** tickets `ready` com todos os bloqueios `merged` e sem `files` em comum com os ativos,
  até o limite de concorrência; o excedente espera.

  ```bash
  tchelvis loop run <workflow> --input-file <caminho absoluto do ticket> --detach
  ```

  Grava `status: running` e `run_id`, e **encerra o turno**.
- **Time:** o do ticket; sem ele, `fast-worker-reviewer` (simples), `worker-reviewer-critic`
  (delicado), `planner-worker-tester-critic` (testes no centro), `designer-worker-reviewer`
  (solução a projetar). Modelos vêm da configuração; o revisor não deve ser mais fraco que o worker.
- **Ao acordar** (um aviso por loop): `orch check` e `loop result`.
  - `PASS` e `ARQUIVOS` dentro de `files`: merge com hooks desligados
    (`git -c core.hooksPath=/dev/null merge --no-ff <branch>`), `status: merged`, dispara os
    liberados.
  - `PASS` com arquivo fora de `files`: para e pergunta.
  - Outro status, ou conflito (desfeito com `merge --abort`): `status: failed`, não dispara nada
    novo, espera os ativos e reporta em até 6 linhas (ticket, `RUN_ID`, relatório).
- **Testes do conjunto** rodam no ticket de integração, dentro do sandbox. O coordenador não os
  executa.
- **Fim:** revisão independente da branch (`orch launch`, fora do contexto do coordenador) e relato.
  Juntar na `main` é decisão do usuário.
- **Sessão perdida:** reconstrói pelos cabeçalhos e por `loop list --runs`; `running` sem loop ativo
  é conferido com `loop status` antes de qualquer novo disparo.

## 3. Instalação

`tchelvis setup` instala `to-tickets` onde já instala `orchestration-tchelvis` (mais o comando do
OpenCode). Hoje o `setup` **sobrescreve** o arquivo de destino. Para `to-tickets` o comportamento é
outro: se já existe uma que não veio do `tchelvis` (não está no manifesto, ou foi editada depois),
não toca e avisa. `uninstall` remove só a nossa.

## Verificado em 2026-11-24

- Loop disparado de um worktree partiu do `HEAD` da integração, e o merge com hooks desligados
  deixou o checkout principal intacto.
- `.loops/` não conta como repositório sujo; `--input-file` aceita o ticket.
- O limite padrão é 2 loops; o excedente é recusado, não enfileirado.

## Limites conhecidos

- O `PASS` é a opinião do revisor; o comando de verificação só é garantido com os `checks` da fase B.
- O loop só usa `claude`, `agy` e `opencode`.
- O worktree e os registros das execuções em `.loops/runs` dele somem juntos.

## Validação

Testes de conteúdo da skill e do `setup` (instala, não sobrescreve a do usuário, desinstala). Ensaio
real: uma spec pequena com 3 tickets, um deles dependente, e um caso de falha.
