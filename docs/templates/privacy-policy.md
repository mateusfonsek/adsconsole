<!--
AdsConsole privacy policy template.

Meta and Google require a public privacy policy URL for the app you create to use
AdsConsole. Copy this file, replace the fields between {{ }} and publish it on any site
of yours (GitHub Pages, Cloudflare Pages, Netlify...). Then paste the URL into:
- Meta: App settings → Basic → Privacy Policy URL
  (and, under "User data deletion", the same URL with #delete-data).
- Google Cloud: Google Auth Platform → Branding → Privacy policy link.

Fields:
- {{OWNER}}: your name or your company's.
- {{EMAIL}}: contact email for data requests.
- {{URL}}: the address where this page will be published.
- {{DATE}}: date of the last update.

Delete this comment before publishing.
-->

# AdsConsole privacy policy

*Last updated: {{DATE}}*

This policy covers the app that {{OWNER}} uses with AdsConsole, open source software that runs **entirely on the computer of whoever installs it**. There is no AdsConsole server: no user data is received, stored or seen by third parties.

## Who controls the data

{{OWNER}} installs AdsConsole and connects **their own** Meta app and **their own** Google Cloud project. The data stays under {{OWNER}}'s control, on the computer where AdsConsole runs.

## What data the app uses

- **Credentials:** Meta App ID, App Secret and access tokens; Google client id, client secret and refresh token. They are kept in the operating system keychain (or, without a keychain, in a file readable only by the user) and never leave the computer, except when sent to the Meta and Google APIs themselves to authenticate.
- **Ad data:** accounts, campaigns, audiences, creatives and metrics, read and changed at the user's request. They are kept in a local database (change history and account cache) and in local logs, with secrets masked.

## Who the data is shared with

Nobody beyond the platforms themselves. The app only talks to:

- the Meta Graph API and login (`graph.facebook.com`, `www.facebook.com`);
- the Google Ads API and login (`googleads.googleapis.com`, `oauth2.googleapis.com`, `accounts.google.com`).

There is no telemetry, analytics, advertising or sale of data.

<a id="delete-data"></a>

## How to delete the data

- **In the app:** removing a connection deletes its credentials from the keychain.
- **Everything:** deleting the AdsConsole data folder (or the one set in `ADSCONSOLE_DATA_DIR`) removes the local database and logs.
- **On Meta:** in Facebook Settings → Security and login → Apps and websites (or Business integrations), removing the app revokes its access.
- **On Google:** at https://myaccount.google.com/permissions, remove the app's access.

## Contact

Questions and requests: {{EMAIL}}. Published version: {{URL}}
