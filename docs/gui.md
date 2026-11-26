# Desktop GUI

`sk1ll-gui` is a Tauri v2 desktop app. Release builds target x86_64 Linux. It talks only to the local daemon over its socket, so the daemon must be running. Without a fleet, it manages a standalone repo. It supports light, dark and system appearance.

## First run

1. **Start the daemon.** If the daemon isn't running, the app offers to start it. With a systemd user manager, it installs `deploy/sk1ll-daemon.service` with `ExecStart` pointing at the `sk1ll` it found, then enables and starts it. An existing systemd unit is used as it is. Without a user service manager, it runs `sk1ll daemon` in the background until you log out. The background process logs to `~/.cache/sk1ll/daemon.log`. See [Run the daemon](fleet.md#run-the-daemon) for service logs and commands. It looks for `sk1ll` next to the app, then on `PATH`, then in `~/.local/bin` and `~/.cargo/bin`. With a custom `SK1LL_DAEMON_SOCKET`, it only shows the commands.
2. **Choose how to use this machine.** While the machine has no repo, a setup wizard replaces the app:
   - *Only this machine* creates a standalone repo.
   - *Join a fleet* takes the server URL and a join code from another machine.
   - *Join as the first Machine* takes the URL and admin token of a [deployed Server](fleet.md#deploy-the-server). The daemon uses the token once to create a join code, joins with it, and doesn't store the token.
3. **Choose harnesses.** The wizard lists the detected harnesses and saves the ones you turn off as `enabled = false` in `machine.toml`. It waits for each choice to save before moving on and shows any save error on this step. It offers the opt-in presets it finds as on, and enables the ones you keep when setup ends (see [Opt-in presets](configuration.md#opt-in-presets)). Later, change them in Settings, which previews what each change leaves in the harness (see [Changing harnesses](configuration.md#changing-harnesses)).
4. **Pick a starting assignment.** A machine that joins without copying another machine's assignment starts with everything off. The app then offers to turn on everything, including Baseline items the machine hasn't pulled yet and the defaults for items added later, or to open the assignment matrix. If a harness fails to save or sync, it lists the failures before you continue.

`sk1ll setup` asks the same questions on the command line. It goes through the daemon if one is running, otherwise through the engine it runs itself, and at the end it offers to install and start the service. Answers come from standard input, so it can be scripted.

After the wizard, the Library shows a getting-started checklist: add a skill or rule, adopt unmanaged skills (optional), commit the draft and, on a fleet, add another machine. The commit step is done once this Machine commits a Draft in standalone mode or pushes a commit to the fleet. It goes away when every required step is done or you dismiss it. Choose **Show getting started** in the command palette to bring it back, including on Machines set up before the checklist existed.

## Screens

To copy an Assignment within a Fleet, open the target Machine's header menu in the Assignment matrix and choose **Copy Assignment from…**. Select a source Machine and confirm the Harness pairs. Matching names are selected by default. Replace copies defaults and explicit toggles, while Merge fills unset toggles and keeps the target defaults. Preview the change count before confirming. If either Assignment changes, the dialog reloads the Harness mapping and asks you to preview again. You can undo individual changes in the Assignment log.

The header shows this machine's id, its headline status and whether the daemon and server are connected. If the daemon stops, the app shows a reconnecting screen with start instructions. Revoked machines and machines waiting for assignment get a banner. Fleet data refreshes on daemon events and every 15 seconds. Standalone machines hide Fleet and the assignment log. Their Assignment belongs to this Machine and is saved in `assignment.json`.

| Screen | What you can do |
|---|---|
| Fleet | See each Machine’s status and Scope, create join codes and remove Machines with typed confirmation. Choose a Machine in the command palette to open its details. |
| Library | Browse Personal Skills, Imports, Rules and Unmanaged skills. Edit Personal Skill text files and Rules in place or open them in an external editor. Use Ctrl/Cmd+S to save, then Review in Draft. Unsaved edits stay open if the Daemon disconnects. If a file changed on disk, reload it or review the diff before overwriting. Create, rename and delete Skills and Rules, fork Imports, adopt Unmanaged skills, and compare variants. An adoption that stopped half-way offers Resume adoption. Renaming keeps the item's Assignment across the Fleet and works offline. |
| Assignment | Use **Copy Assignment from…** in a Machine’s header to preview and copy another Machine’s Assignment. Toggle items per harness, grouped by machine, and set the defaults for new skills and rules. Explicit and inherited values look different. Each machine's assignment loads on its own: one that fails shows its error and a Retry in its column, and bulk changes and undo skip it. Writes check the displayed revision. Bulk Undo also checks that each Machine is still under the same registration. The matrix shows the orphan count. **Clear** lists the items, Machines, and Harnesses you can clear before you confirm. |
| Draft | Pick items, read unified diffs, edit the prefilled commit message, push, restore, and pull when Blocked. The Blocked notice lists each clashing item with **Keep mine** and **Restore**, and **Keep mine for all** when there are several. Keep mine confirms first: it pulls the new Baseline and puts your whole file back on top as Draft, overwriting the other Machine's change to it. Afterwards it names what it kept and opens its diff, to review before you commit. If it was interrupted, your versions stay in Recovery: an **Interrupted Keep mine** notice lists each one with its `sk1ll recover adopt` command until you adopt or discard it (and its notes until you choose **Dismiss notes**), and Conflicts → Recovered content lists them with **Adopt**. Restore takes the Baseline's version (yours is kept in Recovery). |
| Conflicts | Adopt or overwrite harness edits, and adopt or discard recovered copies. A recovered copy is marked **Changed** (it holds an edit, so it needs you) or **Unchanged** (as sk1ll set it aside; routine copies are pruned after ten minutes), and offers Adopt only where sk1ll can tell which item it belongs to. The sidebar badge counts harness conflicts and changed copies, as the Fleet table's Conflicts column does |
| Imports | Resolve a link to preview Skills, file warnings, and total sizes. Use **Advanced** to choose a branch, tag or commit. Expand **Upstream changes** on an available update to see up to 50 commits and the changed files in that Skill. Update and Overwrite check the upstream commit shown in the preview. If it moved, review the refreshed preview. If the running Daemon lacks support for reviewed updates, restart it before retrying. If local edits prevent an update, fork the Import to keep them or confirm an overwrite. Additions and updates land in the Draft. A batch tries every selected skill on its own, even after one fails, and lists each as added, not added (with the error) or not attempted (after Stop). It then reads back what reached the draft: a failed response counts as added only if an import with the same name, source, path, ref and locked commit is there. Every retry checks the names with the daemon again first: a name taken meanwhile fails on its own while the rest are added, and a clash from the last check never disables Retry. |
| Log | Browse assignment changes, newest first, and undo any entry |
| Settings | **Sync now** previews the plan and applies a non-forced sync after confirmation. Use **Desktop notifications** to control the Daemon’s alerts. **Preview instructions** on an enabled Harness shows the full proposed file text and diffs against disk. In the Harnesses panel, turn Harnesses on or off, or add, edit or remove them after a preview of what stays in the harness and what sync places. When turning off or removing a Harness would leave what sk1ll placed behind, the preview offers **Take out and turn off** (or **Take out and remove**): it sets the Harness's items off in the Assignment, lets sync take out what it placed (into Recovery; copies edited by hand stay), and changes `machine.toml` only if nothing managed is left. If something stays, Settings lists it with the errors and leaves the Harness as it is, its items off, so Take out can be retried. **Leave in place** makes the change on its own, which never removes anything. Applying asks for a fresh preview if `machine.toml` changed meanwhile. Restart or upgrade the Daemon if it cannot provide a Harness preview fingerprint. Failed placements are listed afterwards. Opt-in presets found on this machine but not enabled are listed as available, with **Enable**. **Add skills** enables Gemini's skills directory. Open `machine.toml` in the preferred editor, including when Settings cannot parse it. Join or rejoin (with a join code, or the admin token for a new fleet), create single-use join codes and copy connection and version details with **Copy diagnostics** |

In standalone mode, commits stay local. Push and Pull use the repo's own `origin`; without an origin those actions are unavailable. If there is no repo yet, the setup wizard creates it. The repo comes from `SK1LL_REPO`, the nearest `sk1ll.toml` above the daemon's working directory, or the default repo location, `~/.local/share/sk1ll/repo`.

Choose **Action: Sync now** in the command palette to review planned creates, updates, removals, and conflicts. **Apply** checks the reviewed plan. If the plan changed, the dialog shows the new plan and asks you to confirm again. Resolve conflicts in the Conflicts screen.

Items that aren't in the applied baseline carry an `Unpushed` or `Draft only` badge, and you can only assign them on this machine.

Assignment toggles update the screen before the daemon replies and roll back if it reports an error. Draft actions record the selected items and their content fingerprints, so if a selected item changes before you confirm, the app asks you to review it again. The GUI never retries a mutation on its own.

In the Library, a deletion affects this Machine's Draft immediately. Other Machines pick up the deletion after you commit and push it and they sync.

In a Fleet, create a join code from Fleet, Settings or the getting-started checklist. Copy the code or the ready-to-paste `sk1ll join <server-url> <code>` command. Before creating it, choose what the new Machine may manage: **Whole Fleet** (create join codes, change any Machine's Assignment, undo any log entry, remove any Machine) or **Only itself** (its own Assignment, log entries for itself, and removing itself). See [Scopes](fleet.md#scopes). The code works once, and the dialog shows exactly when it expires (24 hours by default, set by the Server's [`SK1LL_SERVER_JOIN_CODE_TTL`](fleet.md#machine-status)). The Fleet screen marks Self-only Machines and shows each Machine's scope in its details. A Self-only Machine can't create join codes and only offers Remove for itself.

The Imports screen marks Imports the Server didn't check under its [import check settings](fleet.md#import-checks) as *Not checked by Server*, with the reason. **Check now** checks them from this Machine.

When an upgrade leaves the app and Daemon on different local API versions, the connection screen offers **Restart daemon** for a running systemd user service on the default socket. It shows terminal instructions for other setups.

The header warns about versions too:

- **Server is out of date** or **This app is out of date**: the Server speaks another protocol, so the Daemon can't connect. The banner names the Server's release and which side to upgrade. For an old Server it gives the `docker compose` command that runs this Machine's release.
- **The Server runs an older release**: the two still work together. Dismiss it to hide it until the Server changes release.
- **The Daemon still runs …**: the Daemon is on an older release than the app, for example after an update it hasn't restarted into. **Restart daemon** restarts the user service.

## Updates

The release `.deb` updates itself from the GitHub releases. It checks once at startup. Turn that off, or check by hand, under Settings → App updates, or choose **Action: Check for app updates** in the command palette. A newer release shows in the header with its release notes. **Update and restart** downloads the package, checks its signature, installs it (the system asks for your password), restarts the Daemon's user service into the new `sk1ll`, and relaunches the app. With unsaved Library edits, it asks before discarding them. **Skip this version** hides that release until a newer one comes out. A Daemon that isn't the user service keeps running the old release, and the app offers **Restart daemon** once it relaunches. Other builds, such as `pnpm tauri dev` or `--no-bundle`, never check.

`sk1ll status` in a terminal says when a newer release is out. It asks GitHub at most once a day and caches the answer. Set `SK1LL_NO_UPDATE_CHECK=1` to turn that off.

## Run it

Install Rust, pnpm and the Tauri v2 prerequisites. That includes GTK3 and WebKitGTK 4.1. Then start the daemon (or let the app start it) and the app:

```sh
systemctl --user start sk1ll-daemon    # or: sk1ll daemon
cd crates/sk1ll-gui/ui
pnpm install
pnpm tauri dev
```

Set `SK1LL_DAEMON_SOCKET=/absolute/path/to/daemon.sock` to use a different daemon. Set `SK1LL_GUI_TRACE=1` to log command names and outcomes, without payloads.

To try it against a throwaway fleet, use `scripts/dev-fleet.sh --gui` (see [Development](development.md#isolated-dev-fleet)).

## Build

```sh
cd crates/sk1ll-gui/ui
pnpm build                      # the app embeds ui/dist, so build this first
pnpm tauri build --no-bundle    # produces target/release/sk1ll-gui
```

The release `.deb` adds `src-tauri/tauri.release.conf.json`, which bundles the `sk1ll` CLI next to the app. Copy a release `sk1ll` to `src-tauri/binaries/sk1ll-<target triple>` first:

```sh
cargo build --release -p sk1ll-cli
mkdir -p ../src-tauri/binaries
cp ../../../target/release/sk1ll ../src-tauri/binaries/sk1ll-$(rustc --print host-tuple)
pnpm tauri build --config src-tauri/tauri.release.conf.json   # target/release/bundle/deb/
```

## Layout

- `crates/sk1ll-gui/src-tauri` is the `sk1ll-gui` workspace member. It holds thin commands that forward to the daemon client and one task that forwards the daemon's server-sent events and reconnects. The Unix gating lives here.
- `crates/sk1ll-gui/ui` is React, TypeScript, Vite, Tailwind and shadcn/ui, built with pnpm. `ui/src/types.ts` is the hand-written copy of the daemon's serde types.
