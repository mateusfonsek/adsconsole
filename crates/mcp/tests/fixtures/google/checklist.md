# Checklist do Google Ads na conta real (API v26)

Registro da execução de `google_checklist` (Task 17 do Plano 2). Sem ids, nomes ou
resource names reais da conta: use `<conta>`, `<campanha>` etc. no lugar.

- Data da execução: 2026-10-18 (seis execuções; as falhas de cada uma foram corrigidas com teste antes da seguinte)
- Nível de acesso da conexão: Explorer
- Comando: `ADSCONSOLE_DATA_DIR=<dados> cargo run -p adsconsole-mcp --example google_checklist -- <conta> [--video <arquivo>]`
- Etapas rodadas: todas (execuções 1 e 2); `leituras,validacao,search,demand_gen` (3); `validacao` (4 e 5); `demand_gen` (6)

## Resultados

Resultado final de cada passo, depois das correções.

| Resultado | Etapa | Ferramenta | Código | Detalhe |
|---|---|---|---|---|
| OK | leituras | list_accounts, get_campaigns, get_search_terms, get_recommendations, get_conversion_actions, list_changes | - | |
| OK | leituras | get_change_history | - | exigia limite superior em `change_date_time` (corrigido) |
| OK | leituras | gaql_search | - | página 1 e cursor |
| PULADO | leituras | keyword_ideas, keyword_metrics | ACCESS_LEVEL_INSUFFICIENT | esperado: exigem nível Basic |
| OK | validacao | create_search_campaign, create_pmax_campaign, create_display_campaign | - | validateOnly |
| OK | validacao | create_demand_gen_campaign | - | multi-asset + carrossel (corrigidos: CTA da lista fixa, imagens distintas por card, ordem das operações) |
| PULADO | validacao | update_asset_group, apply/dismiss_recommendation | - | a conta não tem asset group nem recomendações |
| OK | search | create_search_campaign | - | pausada, início futuro (corrigido: `start_date_time`) |
| OK | search | get_campaign_details | - | corrigido: `campaign.id` no SELECT |
| OK | search | update_campaign (nome, orçamento, end_date, start_date), update_ad_group, update_ad | - | |
| OK | search | manage_keywords (add, pause, set_bid) | - | |
| OK | search | manage_negatives (add, create_list com e sem campanha, attach_list) | - | corrigido: campaignSharedSet sem resourceName |
| OK | search | manage_targeting (local, horário, set_bid_modifier) | - | corrigido: critérios sem resourceName |
| OK | search | undo_change de cada escrita, em ordem inversa; UNDO_CONFLICT no conflito | - | |
| OK | demand_gen | create_demand_gen_campaign + undo_change | - | campanha REMOVED |
| OK | display | create_display_campaign + undo_change | - | campanha REMOVED |
| OK | asset_texto | upload_asset (texto) | - | irreversível |
| OK | video | upload_video + undo_change | - | PROCESSED → UNAVAILABLE |

## Por ferramenta

| Ferramenta | OK | FALHA | PULADO | Códigos |
|---|---|---|---|---|
| leituras (9 ferramentas) | 7 | 0 | 2 | ACCESS_LEVEL_INSUFFICIENT (esperado) |
| criações (4 tipos) | 4 | 0 | 0 | - |
| edições e undo | todas | 0 | 0 | UNDO_CONFLICT (esperado) |
| update_asset_group, recomendações | 0 | 0 | 3 | sem dados na conta |

## Itens VERIFY(v26)

| Item | O que se verificou | Resultado | Evidência (sanitizada) |
|---|---|---|---|
| T9.1 | AdOperation update em `ads/{id}` e leitura FROM ad | confirmado | update aplicado; snapshot e depois lidos FROM ad |
| T9.2 | chaves só de saída do RSA no desfazer | confirmado | desfazer do update_ad sem UNDO_CONFLICT |
| T9.3 / T13.2 | datas da campanha | **corrigido** | v26 só tem `start_date_time`/`end_date_time` (`"yyyy-MM-dd HH:mm:ss"`); `startDate` dá "Unknown name" |
| T9.4 | desfazer de end_date que era nulo | confirmado | depois de limpar, `endDateTime` volta ausente (sem data sentinela) |
| T10.1 | temporários compostos em create_list | **corrigido** | `campaignSharedSets/<c>~-1` dava DUPLICATE_TEMP_IDS; agora sem resourceName |
| T10.2 | attach_list sem resourceName | confirmado | aplicado |
| T11.1 | desfazer set_bid_modifier nulo | confirmado | critério volta sem `bidModifier` |
| T12.1 / T12.2 | asset group e recomendações | não exercitado | a conta não tem |
| T13.1 | filhos criados sem resourceName | confirmado | ad group, keywords e anúncio encontrados |
| T14.1 | PMax assetAutomationSettings e brandGuidelinesEnabled | confirmado | validateOnly aceitou |
| T14.2 | assets de imagem `img-<hash>` | confirmado | nomes `img-<16hex>` na conta |
| T14.3 | sinais de público no PMax | não exercitado | a conta não tem públicos |
| T15.1–T15.5 | Demand Gen (local/idioma no grupo, grupo sem type, businessName, CTA, card) | confirmado após correções | CTA só da lista fixa do Google; imagens únicas entre cards; um temporário não pode ser referenciado por operação do mesmo lote de operações seguidas do mesmo tipo |
| T15.6 | Display sem networkSettings, RDA | confirmado | criação real aplicada |
| T16.1 | nome do upload de vídeo | confirmado | resultado com upload e asset; PROCESSED |
| T16.2 | origem da URL da sessão | confirmado | mesma origem de upload_base |

## Smoke com o Claude Code real

`google_server` com `ADSCONSOLE_AUTO_APPROVE=1` (modo Approval) e `claude -p --mcp-config <tmp> --strict-mcp-config`.

| Pedido | Ferramenta | Resultado |
|---|---|---|
| listar contas | list_accounts | OK |
| listar campanhas | get_campaigns | OK |
| criar campanha de pesquisa pausada | create_search_campaign | OK (applied) |
| desfazer a criação | undo_change | OK (campanha e orçamento removidos) |
| conferir | get_change_status | OK (`undone`, com `undone_by`) |

## O que ficou na conta

- Objetos `REMOVED`: as campanhas de teste (Search, Demand Gen, Display e a do smoke) e seus orçamentos, ad groups, critérios, listas compartilhadas e vínculos.
- Assets (irreversíveis): imagens de teste `img-<hash>` (Demand Gen/Display/PMax), assets de texto e os assets dos vídeos (os uploads no YouTube ficaram UNAVAILABLE).
- Nenhuma campanha ativa; nada veicula.
