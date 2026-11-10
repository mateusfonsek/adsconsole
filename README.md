# AdsConsole

Desktop app that connects your AI agent (Claude Code, Claude Desktop, Codex, Gemini CLI, Cursor) to your Google Ads and Meta Ads accounts. The AI reads accounts, campaigns and metrics and proposes changes. Every write goes through your approval in the app.

- **Local:** there is no AdsConsole server. Credentials live in the system keychain and data in a local SQLite database.
- **Your credentials:** you use your own Meta app and your own Google Cloud project.
- **Controlled writes:** each platform has a write mode (read-only, approval or direct). The default is approval, with history and undo.
- **Bilingual UI:** the app interface is available in English and Brazilian Portuguese (switch it in **Settings**).

## Install

Download the installer from the [latest release](../../releases/latest):

- **macOS:** `.dmg` (Apple Silicon or Intel). The app is not signed by Apple yet. On first launch macOS blocks it: open **System Settings → Privacy & Security** and click **Open Anyway** next to AdsConsole (on macOS 14 or earlier, right-click the app and choose **Open**).
- **Windows:** `.msi` or `.exe`. At "Windows protected your PC", click **More info → Run anyway**.
- **Linux:** `.AppImage` (make it executable) or `.deb`.

The app tells you when a new version is out.

## Getting started

1. **Connect a platform.** In **Connections**, the Google wizard asks for the OAuth client JSON (type **Desktop app**, in a project with the Google Ads API enabled). The Meta wizard asks for the App ID, the App Secret and the Facebook Login for Business `config_id`. Both have direct links to each configuration screen.
2. **Connect your AI client.** In **MCPs**, pick the client and copy the ready-made snippet (the command or JSON already carries the address and the token).
3. **Use it.** Ask the AI for something, like "list my campaigns". When it asks for a change, the system notifies you and you approve or reject it in **Approvals**.

AdsConsole lives in the menu bar (or system tray) with the number of pending approvals. Closing the window does not stop the MCPs.

## Pages you need to publish

Meta and Google require public URLs for your app. The templates are in [`docs/templates/`](docs/templates/):

- [`privacy-policy.md`](docs/templates/privacy-policy.md): privacy policy. Fill in the `{{ }}` fields, publish it on any site of yours and paste the URL into Meta (Privacy Policy URL and data deletion instructions) and into Google Cloud (consent screen Branding).
- [`meta-oauth-relay.html`](docs/templates/meta-oauth-relay.html): Meta login return page, required once the app is published (Live mode), because Meta does not accept `localhost` in that mode. Publish it at an `https` address, add it to the app's "Valid OAuth Redirect URIs" and enter it in the Meta wizard.

## Advanced configuration

The data folder (`~/Library/Application Support/adsconsole` on macOS) holds `config.json`, the local database and the logs. The app edits everything through its screens, but environment variables take precedence: `ADSCONSOLE_DATA_DIR`, `ADSCONSOLE_PORT`, `ADSCONSOLE_TOKEN`.

## Development

Requirements: Rust 1.95 (`rust-toolchain.toml` installs the right version through `rustup`) and Node 22. On Linux, the Tauri dependencies: `libwebkit2gtk-4.1-dev libappindicator3-dev librsvg2-dev libxdo-dev patchelf`.

```sh
npm --prefix ui ci
cd crates/app && npx --prefix ../../ui tauri dev               # app with reload
cd crates/app && npx --prefix ../../ui tauri dev -- -- --demo  # sample data, nothing leaves the computer
```

Tests:

```sh
cargo test --workspace
npm --prefix ui test
npm --prefix ui run build && npm --prefix ui run e2e
```

After changing a command in `crates/app`, regenerate the UI types with `npm --prefix ui run bindings`.

Command-line examples (useful for testing against real accounts without the app):

```sh
cargo run -p adsconsole-mcp --example google_connect -- path/client.json "My account"
cargo run -p adsconsole-mcp --example meta_connect -- "My account" --login <app_id> <config_id> [--relay <url>]
cargo run -p adsconsole-mcp --example google_server
cargo run -p adsconsole-mcp --example meta_server
```

See [`CONTRIBUTING.md`](CONTRIBUTING.md) before sending changes. To publish a version, follow [`RELEASING.md`](RELEASING.md).

## License

[GNU AGPL-3.0](LICENSE). You may use, modify and redistribute AdsConsole, as long as you keep the authorship notices and publish the source code of your versions under the same license, including when you offer them as a network service.
