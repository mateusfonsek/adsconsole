# How to publish a release

1. Update the version in `crates/app/Cargo.toml`, `crates/app/tauri.conf.json` and `ui/package.json`.
2. Run the tests: `cargo test --workspace`, `npm --prefix ui test` and `npm --prefix ui run e2e`.
3. Create the `vX.Y.Z` tag on `main` and push it (`git push origin vX.Y.Z`). The **Release** workflow builds the installers and creates a draft release.
4. Go through the checklist below with the draft's installers.
5. Publish the draft. From then on, the installed app announces the new version.

## Manual checklist (macOS, Windows and Linux)

- [ ] Install and open for the first time on a clean Mac with the `.dmg` downloaded from the release (Gatekeeper: System Settings → Privacy & Security → Open Anyway; it must not say "damaged"), and on Windows (SmartScreen).
- [ ] Connect Google Ads with a real project (wizard, client JSON, browser login).
- [ ] Connect Meta Ads with a real app, by login and by token.
- [ ] Have the AI request a write in approval mode: the notification arrives, the counter shows in the menu bar, approving applies it.
- [ ] Undo the change from History.
- [ ] Pause and resume an MCP from the menu bar.
- [ ] Close the window (the MCPs keep running) and reopen it from the menu bar.
- [ ] Turn on "Open at login", restart the computer and check.
- [ ] Quit with a pending approval: the app asks for confirmation.
- [ ] Switch language and theme.
- [ ] AI clients: Claude Code, Claude Desktop, Codex, Gemini CLI and Cursor connect with the copied snippet and list the tools.
