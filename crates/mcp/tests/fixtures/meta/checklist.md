# Checklist da Meta Ads na conta real (Graph API v26.0)

Registro da execução de `meta_checklist` (Task 13 do Plano 3). Sem ids, nomes ou
tokens reais da conta: use `<conta>`, `<campanha>` etc. no lugar.

- Data da execução: 2026-10-19 e 2026-10-20
- Tier da Marketing API (`ads_api_access_tier`): Limited (app em modo de desenvolvimento)
- Comandos:
  - `ADSCONSOLE_DATA_DIR=<dados> cargo run -p adsconsole-mcp --example meta_checklist -- <conta> --page <página> --image <imagem>`
  - `ADSCONSOLE_DATA_DIR=<dados> cargo run -p adsconsole-mcp --example meta_checklist -- <conta> --sem-anuncio --only criacao,orcamento,duplicar`
- Etapas rodadas: leituras, imagem, validacao, criacao, rollback, orcamento, duplicar
  (e, em rodadas anteriores, video e criativo_novo)
- Contas: as primeiras rodadas foram em contas sem forma de pagamento válida (edição,
  orçamento e duplicação com `--sem-anuncio`). A rodada final (2026-10-20) foi numa conta
  pessoal com cartão, Página própria e o **app publicado**: 48 OK, 0 FALHA.

## Resultados

| Resultado | Etapa | Ferramenta | Código | Detalhe |
|---|---|---|---|---|
| OK | leituras | list_ad_accounts | - | 5 contas |
| OK | leituras | get_campaigns | - | 50 itens (paginado) |
| OK | leituras | get_insights | - | 0 itens |
| OK | leituras | search_targeting | - | 4 tipos, 25 itens cada |
| OK | leituras | estimate_audience | - | ready, faixa de usuários |
| OK | leituras | get_pages | - | 2 páginas; usando `--page` |
| OK | leituras | get_page_posts | - | 2 posts; um promovível |
| PULADO | leituras | search_ad_library | - | a Biblioteca exige confirmação de identidade |
| OK | imagem | upload_image | - | enviada e apagada no fim (desfazer por hash) |
| OK | validacao | create_campaign | - | validate_only aceito |
| FALHA | criacao | create_campaign | API_ERROR | "Nenhuma forma de pagamento" no anúncio (conta sem pagamento) |
| OK | rollback | create_campaign | - | falhou no anúncio e apagou 3 objetos |
| OK | criacao | update_campaign / update_adset | - | nome, orçamento e público (sem anúncio) |
| OK | criacao | undo_change | - | UNDO_CONFLICT em edição sobrescrita; as 4 edições desfeitas |
| OK | orcamento | update_adset | - | 4 mudanças aceitas; 5ª recusada com BUDGET_CHANGE_LIMIT |
| OK | duplicar | duplicate | - | cópia síncrona pausada; desfeita |
| FALHA | criativo_novo | create_campaign | API_ERROR | subcódigo 1885183: app em desenvolvimento não anuncia post criado por ele |
| OK | video | upload_video | - | enviado e processado; não dá para apagar pela API (#10) |
| OK | campo_vazio | update_campaign | - | spend_cap preenchido (mínimo R$ 300): sem desfazer, com aviso; desfazer recusado com NOT_REVERSIBLE |
| OK | login | meta_connect --login | - | troca do código em form POST e debug_token com Bearer (depois da revisão) |
| FALHA | criacao | create_campaign | API_ERROR | numa conta com saldo pré-pago: código 31/3858385 "Autentique sua conta" no anúncio (veio em HTTP 200) |

### Rodada final (conta pessoal com cartão, app publicado)

| Resultado | Etapa | Ferramenta | Detalhe |
|---|---|---|---|
| OK | criativo_novo | create_campaign | criativo de imagem nova + texto + link aceito com o app publicado; desfeito |
| OK | criacao | create_campaign / get_campaign_details | campanha + ad set + anúncio (post existente) |
| OK | criacao | update_campaign / update_adset / update_ad | edições e desfazer de cada uma |
| OK | criacao | create_ad | 2º anúncio no ad set com criativo existente; desfeito |
| OK | criacao | undo_change | UNDO_CONFLICT; campanha DELETED depois do desfazer |
| OK | rollback | create_campaign | falha no 2º ad set apagou campanha, ad set, criativo e anúncio |
| OK | orcamento | update_adset | 4 mudanças; 5ª recusada com o horário de liberação |
| OK | duplicar | duplicate | cópia pausada de campanha com anúncio; desfeita |
| OK | campo_vazio | update_campaign | spend_cap: sem desfazer, com aviso |

Achado da limpeza: o criativo de post existente recebe da Meta o texto do post como nome, e a
imagem só pode ser apagada depois dos criativos que a usam. O checklist agora apaga os
criativos que surgiram na rodada e só depois a imagem.

## Itens VERIFY(v26.0)

| Item | O que se verificou | Resultado | Evidência (sanitizada) |
|---|---|---|---|
| V1 | login no navegador com porta aleatória em localhost | OK | conexão criada pelo `meta_connect --login` |
| V2 | `targeting_automation.advantage_audience` no ad set | OK | ad set criado com `advantage_audience=0` |
| V3 | `is_adset_budget_sharing_enabled` sem orçamento de campanha | OK | validate_only aceitou `false` |
| V4 | criativo de imagem com `link_data` e botão | OK | aceito com o app publicado (em desenvolvimento: 1885183) |
| V5 | anúncio com `creative {creative_id}` | OK | anúncio criado e editado (conta com cartão) |
| V6 | desfazer/apagar campanha apaga os filhos | OK | campanha lida como DELETED; rollback apagou campanha, ad set e criativo |
| V7 | 4 mudanças de orçamento por hora no ad set | OK | 5ª recusada localmente |
| V8 | cópia síncrona com `/copies` | OK | `deep_copy`, `status_option=PAUSED`, `rename_options` aceitos |
| V9 | criativo novo conforme o modo do app | OK | em desenvolvimento: 1885183; publicado: aceito |
| V10 | criativo de post existente (`object_story_id`) | OK | anúncio criado com o post; exige `promoted_object {page_id}` e `ON_POST` no ad set |
| — | engajamento com post | — | o ad set precisa de `promoted_object {page_id}` e `destination_type=ON_POST` |
| — | apagar mídia | — | imagem: `POST {act}/adimages` com `hash` e `method=delete` funciona; vídeo: #10 em todas as formas |

## Smoke com o Claude Code real

`claude -p --strict-mcp-config --mcp-config <arquivo>` com o `meta_server` (modo Approval), só leituras.

| Pedido | Ferramenta | Resultado |
|---|---|---|
| Listar contas | list_ad_accounts | 5 contas com nome, id e status |
| Campanhas da conta pessoal | get_campaigns | lista vazia, sem paginação pendente |
| Insights dos últimos 7 dias | get_insights | nenhuma linha |
| Listar Páginas | get_pages | 2 Páginas com permissão de anunciar |

## O que ficou na conta

- Objetos apagados: todas as campanhas, ad sets, criativos e imagens criados pelo checklist
  (conferido com `--list-media`, que lista vídeos, imagens, criativos e campanhas).
- Mídias e criativos (irreversíveis): 1 vídeo de teste de uma rodada anterior na conta
  pessoal (a Meta não deixa apagá-lo pela API; mantido por decisão do dono da conta).
