# Google Ads checklist on the real account (API v26)

Record of the `google_checklist` run. No real ids, names or resource names from the
account: `<account>`, `<campaign>` etc. stand in for them.

- Run date: 2026-10-18 (six runs; the failures of each one were fixed, with a test, before the next)
- Connection access level: Explorer
- Command: `ADSCONSOLE_DATA_DIR=<data> cargo run -p adsconsole-mcp --example google_checklist -- <account> [--video <file>]`
- Steps run: all (runs 1 and 2); `reads,validation,search,demand_gen` (3); `validation` (4 and 5); `demand_gen` (6)

## Results

Final result of each step, after the fixes.

| Result | Step | Tool | Code | Detail |
|---|---|---|---|---|
| OK | reads | list_accounts, get_campaigns, get_search_terms, get_recommendations, get_conversion_actions, list_changes | - | |
| OK | reads | get_change_history | - | required an upper bound on `change_date_time` (fixed) |
| OK | reads | gaql_search | - | page 1 and cursor |
| SKIPPED | reads | keyword_ideas, keyword_metrics | ACCESS_LEVEL_INSUFFICIENT | expected: they require Basic level |
| OK | validation | create_search_campaign, create_pmax_campaign, create_display_campaign | - | validateOnly |
| OK | validation | create_demand_gen_campaign | - | multi-asset + carousel (fixed: CTA from the fixed list, distinct images per card, order of the operations) |
| SKIPPED | validation | update_asset_group, apply/dismiss_recommendation | - | the account has no asset group or recommendations |
| OK | search | create_search_campaign | - | paused, future start (fixed: `start_date_time`) |
| OK | search | get_campaign_details | - | fixed: `campaign.id` in the SELECT |
| OK | search | update_campaign (name, budget, end_date, start_date), update_ad_group, update_ad | - | |
| OK | search | manage_keywords (add, pause, set_bid) | - | |
| OK | search | manage_negatives (add, create_list with and without campaign, attach_list) | - | fixed: campaignSharedSet without resourceName |
| OK | search | manage_targeting (location, schedule, set_bid_modifier) | - | fixed: criteria without resourceName |
| OK | search | undo_change of each write, in reverse order; UNDO_CONFLICT on the conflict | - | |
| OK | demand_gen | create_demand_gen_campaign + undo_change | - | campaign REMOVED |
| OK | display | create_display_campaign + undo_change | - | campaign REMOVED |
| OK | text_asset | upload_asset (text) | - | irreversible |
| OK | video | upload_video + undo_change | - | PROCESSED → UNAVAILABLE |

## By tool

| Tool | OK | FAIL | SKIPPED | Codes |
|---|---|---|---|---|
| reads (9 tools) | 7 | 0 | 2 | ACCESS_LEVEL_INSUFFICIENT (expected) |
| creations (4 types) | 4 | 0 | 0 | - |
| edits and undo | all | 0 | 0 | UNDO_CONFLICT (expected) |
| update_asset_group, recommendations | 0 | 0 | 3 | no data in the account |

## VERIFY(v26) items

| Item | What was verified | Result | Evidence (sanitized) |
|---|---|---|---|
| V1 | AdOperation update on `ads/{id}` and read FROM ad | confirmed | update applied; snapshot and after state read FROM ad |
| V2 | output-only RSA keys on undo | confirmed | undo of update_ad without UNDO_CONFLICT |
| V3 / V11 | campaign dates | **fixed** | v26 only has `start_date_time`/`end_date_time` (`"yyyy-MM-dd HH:mm:ss"`); `startDate` gives "Unknown name" |
| V4 | undo of an end_date that was null | confirmed | after clearing, `endDateTime` comes back absent (no sentinel date) |
| V5 | composite temp ids in create_list | **fixed** | `campaignSharedSets/<c>~-1` gave DUPLICATE_TEMP_IDS; now without resourceName |
| V6 | attach_list without resourceName | confirmed | applied |
| V7 | undo of a null set_bid_modifier | confirmed | criterion comes back without `bidModifier` |
| V8 / V9 | asset group and recommendations | not exercised | the account has none |
| V10 | children created without resourceName | confirmed | ad group, keywords and ad found |
| V12 | PMax assetAutomationSettings and brandGuidelinesEnabled | confirmed | validateOnly accepted |
| V13 | image assets `img-<hash>` | confirmed | names `img-<16hex>` in the account |
| V14 | audience signals on PMax | not exercised | the account has no audiences |
| V15–V19 | Demand Gen (location/language on the group, group without type, businessName, CTA, card) | confirmed after fixes | CTA only from Google's fixed list; unique images across cards; a temp id cannot be referenced by an operation in the same batch of consecutive operations of the same type |
| V20 | Display without networkSettings, RDA | confirmed | real creation applied |
| V21 | video upload name | confirmed | result with upload and asset; PROCESSED |
| V22 | origin of the session URL | confirmed | same origin as upload_base |

## Smoke test with the real Claude Code

`google_server` with `ADSCONSOLE_AUTO_APPROVE=1` (Approval mode) and `claude -p --mcp-config <tmp> --strict-mcp-config`.

| Request | Tool | Result |
|---|---|---|
| list accounts | list_accounts | OK |
| list campaigns | get_campaigns | OK |
| create a paused search campaign | create_search_campaign | OK (applied) |
| undo the creation | undo_change | OK (campaign and budget removed) |
| check | get_change_status | OK (`undone`, with `undone_by`) |

## What was left in the account

- `REMOVED` objects: the test campaigns (Search, Demand Gen, Display and the smoke one) and their budgets, ad groups, criteria, shared lists and links.
- Assets (irreversible): `img-<hash>` test images (Demand Gen/Display/PMax), text assets and the video assets (the YouTube uploads ended up UNAVAILABLE).
- No active campaign; nothing is serving.

## v0.2.0 / PR #1 verification (2026-11-17)

Connection: Explorer. Commands: `google_checklist <account> --only reads,search --yes`,
followed by `google_checklist <account> --only reads` after fixing the history read.
The full run recorded 61 OK and one failure in `get_change_history`; the final read-only
run recorded 10 OK, zero failures and three expected skips (no campaign and the two
Basic-only keyword tools).

| Item | Real API result |
|---|---|
| V23 labels | Create and update; attach to campaign, ad group, ad and keyword; detach and undo all passed. The campaign link was read back after restoration. |
| V24 extensions | Campaign callouts and structured snippet, ad group sitelink, unlink and undo all passed. Restored campaign asset links were read back as ENABLED. |
| V25 field catalog | `googleAdsFields:search` returned the filtered campaign attributes. |
| Cleanup | All reversible writes were undone and the paused test campaigns were confirmed REMOVED. Extension assets remain unused in the library, as designed. |

Two issues found during verification were fixed before tagging:

- The V24 evidence query now selects `campaign.id`, required by the real API. Evidence
  query errors count as failures; failures or incomplete cleanup return a nonzero exit code.
- Selecting `change_event.changed_fields` returned HTTP 500 / INTERNAL on actual events,
  while the same query without that field succeeded. `get_change_history` now retries
  without that detail only on this error and returns an explicit `warning`. Other errors
  and a failed fallback remain errors. The final real-account read returned 20 events.

Automated validation: workspace formatting, Clippy with warnings denied, all Rust tests,
UI lint and TypeScript, 45 UI unit tests, production UI build and 12 Playwright tests
(Chromium and WebKit). The npm production dependency audit found no vulnerabilities;
the development dependency audit reported nine moderate findings in the ESLint/minimatch
dependency tree. Clean-machine installer checks were not performed locally.
