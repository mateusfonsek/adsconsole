# How to publish a release

Releases are automatic and happen **only when a version tag is pushed**. Commits pushed to `main` run CI but never publish anything, so work in progress is safe.

1. Update the version in `crates/app/Cargo.toml`, `crates/app/tauri.conf.json` and `ui/package.json` (same `X.Y.Z` in all three), and commit it.
2. Create the tag on that commit and push it:
   ```bash
   git tag vX.Y.Z
   git push origin vX.Y.Z
   ```
3. The **Release** workflow then:
   - checks that the tag matches the version in the three files (it stops if not);
   - runs the same checks as CI (Rust, UI, builds on the three systems);
   - builds the installers for macOS (Apple Silicon and Intel), Windows and Linux;
   - publishes the GitHub release with all installers once every platform succeeded. If any step fails, nothing is published.
4. Installed apps announce the new version on their next check.

Tags with a suffix, such as `v1.2.0-beta.1`, are published as **pre-releases**: they can be downloaded but are not offered as an update.

The installers can only be downloaded by everyone, and the update check only works, while the repository is public.

To undo a bad release, delete it on GitHub and delete the tag (`git push origin :refs/tags/vX.Y.Z`), then fix and tag a new version.

## Manual checklist before tagging a big release (macOS, Windows and Linux)

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
