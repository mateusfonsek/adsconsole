# Meta Ads checklist on the real account (Graph API v26.0)

Record of the `meta_checklist` run. No real ids, names or tokens from the account:
`<account>`, `<campaign>` etc. stand in for them.

- Run date: 2026-10-19 and 2026-10-20
- Marketing API tier (`ads_api_access_tier`): Limited (app in development mode)
- Commands:
  - `ADSCONSOLE_DATA_DIR=<data> cargo run -p adsconsole-mcp --example meta_checklist -- <account> --page <page> --image <image>`
  - `ADSCONSOLE_DATA_DIR=<data> cargo run -p adsconsole-mcp --example meta_checklist -- <account> --no-ads --only creation,budget,duplicate`
- Steps run: reads, image, validation, creation, rollback, budget, duplicate
  (and, in earlier runs, video and new_creative)
- Accounts: the first runs were on accounts without a valid payment method (edit,
  budget and duplication with `--no-ads`). The final run (2026-10-20) was on a personal
  account with a card, its own Page and the **app published**: 48 OK, 0 FAIL.

## Results

| Result | Step | Tool | Code | Detail |
|---|---|---|---|---|
| OK | reads | list_ad_accounts | - | 5 accounts |
| OK | reads | get_campaigns | - | 50 items (paginated) |
| OK | reads | get_insights | - | 0 items |
| OK | reads | search_targeting | - | 4 types, 25 items each |
| OK | reads | estimate_audience | - | ready, user range |
| OK | reads | get_pages | - | 2 pages; using `--page` |
| OK | reads | get_page_posts | - | 2 posts; one promotable |
| SKIPPED | reads | search_ad_library | - | the Library requires identity confirmation |
| OK | image | upload_image | - | uploaded and deleted at the end (undo by hash) |
| OK | validation | create_campaign | - | validate_only accepted |
| FAIL | creation | create_campaign | API_ERROR | "No payment method" on the ad (account without payment) |
| OK | rollback | create_campaign | - | failed on the ad and deleted 3 objects |
| OK | creation | update_campaign / update_adset | - | name, budget and audience (no ad) |
| OK | creation | undo_change | - | UNDO_CONFLICT on an overwritten edit; the 4 edits undone |
| OK | budget | update_adset | - | 4 changes accepted; 5th rejected with BUDGET_CHANGE_LIMIT |
| OK | duplicate | duplicate | - | synchronous paused copy; undone |
| FAIL | new_creative | create_campaign | API_ERROR | subcode 1885183: an app in development does not advertise a post it created |
| OK | video | upload_video | - | uploaded and processed; cannot be deleted through the API (#10) |
| OK | empty_field | update_campaign | - | spend_cap filled (minimum R$ 300): no undo, with a warning; undo rejected with NOT_REVERSIBLE |
| OK | login | meta_connect --login | - | code exchange as a form POST and debug_token with Bearer (after the review) |
| FAIL | creation | create_campaign | API_ERROR | on an account with prepaid balance: code 31/3858385 "Authenticate your account" on the ad (came in HTTP 200) |

### Final run (personal account with a card, app published)

| Result | Step | Tool | Detail |
|---|---|---|---|
| OK | new_creative | create_campaign | new image creative + text + link accepted with the app published; undone |
| OK | creation | create_campaign / get_campaign_details | campaign + ad set + ad (existing post) |
| OK | creation | update_campaign / update_adset / update_ad | edits and undo of each one |
| OK | creation | create_ad | 2nd ad in the ad set with an existing creative; undone |
| OK | creation | undo_change | UNDO_CONFLICT; campaign DELETED after the undo |
| OK | rollback | create_campaign | failure on the 2nd ad set deleted campaign, ad set, creative and ad |
| OK | budget | update_adset | 4 changes; 5th rejected with the release time |
| OK | duplicate | duplicate | paused copy of a campaign with an ad; undone |
| OK | empty_field | update_campaign | spend_cap: no undo, with a warning |

Cleanup finding: the existing-post creative gets the post text from Meta as its name, and the
image can only be deleted after the creatives that use it. The checklist now deletes the
creatives that appeared during the run and only then the image.

## VERIFY(v26.0) items

| Item | What was verified | Result | Evidence (sanitized) |
|---|---|---|---|
| V1 | browser login with a random port on localhost | OK | connection created by `meta_connect --login` |
| V2 | `targeting_automation.advantage_audience` on the ad set | OK | ad set created with `advantage_audience=0` |
| V3 | `is_adset_budget_sharing_enabled` without a campaign budget | OK | validate_only accepted `false` |
| V4 | image creative with `link_data` and button | OK | accepted with the app published (in development: 1885183) |
| V5 | ad with `creative {creative_id}` | OK | ad created and edited (account with a card) |
| V6 | undoing/deleting a campaign deletes the children | OK | campaign read as DELETED; rollback deleted campaign, ad set and creative |
| V7 | 4 budget changes per hour on the ad set | OK | 5th rejected locally |
| V8 | synchronous copy with `/copies` | OK | `deep_copy`, `status_option=PAUSED`, `rename_options` accepted |
| V9 | new creative according to the app mode | OK | in development: 1885183; published: accepted |
| V10 | creative from an existing post (`object_story_id`) | OK | ad created with the post; requires `promoted_object {page_id}` and `ON_POST` on the ad set |
| — | engagement with a post | — | the ad set needs `promoted_object {page_id}` and `destination_type=ON_POST` |
| — | deleting media | — | image: `POST {act}/adimages` with `hash` and `method=delete` works; video: #10 in every form |

## Smoke test with the real Claude Code

`claude -p --strict-mcp-config --mcp-config <file>` with `meta_server` (Approval mode), reads only.

| Request | Tool | Result |
|---|---|---|
| List accounts | list_ad_accounts | 5 accounts with name, id and status |
| Campaigns of the personal account | get_campaigns | empty list, no pending pagination |
| Insights for the last 7 days | get_insights | no rows |
| List Pages | get_pages | 2 Pages with permission to advertise |

## What was left in the account

- Deleted objects: all the campaigns, ad sets, creatives and images created by the checklist
  (checked with `--list-media`, which lists videos, images, creatives and campaigns).
- Media and creatives (irreversible): 1 test video from an earlier run in the personal
  account (Meta does not allow deleting it through the API; kept at the account owner's decision).
