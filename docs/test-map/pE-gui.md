# Test map, phase E (GUI UI tests)

Every per-component test in `crates/sk1ll-gui/ui/src/**/*.test.ts(x)` (19 files) is deleted. Its behavior is either
pinned by a user-flow test in `ui/src/flows/` (the real `App` against the dev mock Daemon, driven with Testing Library
and user-event) or dropped. Names below are the old tests' as `vitest list` printed them (`%s`/`$x` are `it.each`).

New tests, by alias (all in `crates/sk1ll-gui/ui/src/flows/`):

| Alias | Test (file) |
| --- | --- |
| shell:1 | keeps a standalone Machine to its own screens and never asks for Fleet data, even in the background (shell.test.tsx) |
| shell:2 | opens the screen named in the address, the Fleet otherwise, and moves focus to each screen it goes to (shell.test.tsx) |
| shell:3 | jumps anywhere from the command palette, matching every word in any order, and gives focus back when dismissed (shell.test.tsx) |
| shell:4 | refreshes what each Daemon notification touches and leaves the rest alone (shell.test.tsx) |
| setup:1 | sets up a standalone Machine: chooses Harnesses before anything syncs, creates the repo, and follows the checklist (setup.test.tsx) |
| setup:2 | joins a Fleet: warns about plain HTTP, waits out the Daemon restart, then turns everything on and reports what failed (setup.test.tsx) |
| setup:3 | joins with an Assignment copied from another Machine, skipping the starting Assignment (setup.test.tsx) |
| setup:4 | starts a new Fleet as its first Machine with the admin token, keeping the wizard while the join fails (setup.test.tsx) |
| setup:5 | leaves the restart screen once the incompatible Daemon stops answering (setup.test.tsx) |
| setup:screen | connection screen $name, 8 cases (setup.test.tsx) |
| library:1 | filters, searches and sorts the Library, and reads an item's files rendered or raw (library.test.tsx) |
| library:2 | forks an Import and adopts an Unmanaged skill's chosen variant before opening either in an editor (library.test.tsx) |
| library:3 | keeps adoption off while the Server is unreachable, explaining why (library.test.tsx) |
| library:4 | creates, renames and deletes personal items in the Draft, refusing names already taken (library.test.tsx) |
| editing:1 | edits a file in place: guards unsaved edits, saves against what it read, and never silently overwrites a disk edit (editing.test.tsx) |
| draft:1 | commits the reviewed selection and refuses to commit once the Draft changed under the dialog (draft.test.tsx) |
| draft:2 | restores only the reviewed items, even when an edit or a new item arrives while Restore is open (draft.test.tsx) |
| draft:3 | gets past a Blocked pull by keeping this Machine's version, one item or all at once (draft.test.tsx) |
| draft:4 | pushes unpushed work and clears the notes an interrupted Keep mine left (draft.test.tsx) |
| draft:5 | commits to the local repository in standalone, pushing to origin only when asked (draft.test.tsx) |
| draft:6 | keeps Push and Pull off without an origin, and never commits once every reviewed item left the Draft (draft.test.tsx) |
| conflicts:1 | resolves a Harness conflict from the Library banner and discards recovered content only after confirming (conflicts.test.tsx) |
| conflicts:2 | keeps diffs closed when four or more entries need review, loading each on demand (conflicts.test.tsx) |
| imports:1 | checks Imports, previews upstream commits, and updates one into the Draft against the upstream it showed (imports.test.tsx) |
| imports:2 | keeps local edits to an Import by forking it, or overwrites them only after confirming (imports.test.tsx) |
| imports:3 | adds several Skills from one repository, retrying only the one that failed, then reviews them in the Draft (imports.test.tsx) |
| imports:4 | stops a batch on request, and counts a Skill that landed despite an error response instead of adding it twice (imports.test.tsx) |
| imports:5 | adds the one Skill a folder link names, reusing a deleted item's name, and never counts another Import as the one that landed (imports.test.tsx) |
| fleet:1 | reads Fleet health, filters Machines, and removes one after confirming (fleet.test.tsx) |
| fleet:2 | works through this Machine's queue and its Assignments from the Fleet screen (fleet.test.tsx) |
| fleet:3 | shows a Self-only Machine that it can remove only itself and cannot add Machines (fleet.test.tsx) |
| matrix:1 | writes single toggles against the revision shown, and reloads instead when someone changed it first (matrix.test.tsx) |
| matrix:2 | bulk-changes rows and filtered columns once per Machine, and Undo restores what was accepted (matrix.test.tsx) |
| matrix:3 | copies another Machine's Assignment after previewing it, and previews again when the target changed (matrix.test.tsx) |
| matrix:4 | clears orphan toggles across the Fleet, or only its own on a Self-only Machine (matrix.test.tsx) |
| matrix:5 | lets a Self-only Machine clear only its own orphan toggles (matrix.test.tsx) |
| matrix:6 | keeps the other Machines usable while one Assignment fails to load, retrying only that column, and rolls back a refused toggle (matrix.test.tsx) |
| matrix:7 | assigns this Machine's Draft-only items in standalone, without asking for the Fleet (matrix.test.tsx) |
| log:1 | pages through the log, filters it on the Server, and starts again from the newest on every new filter (log.test.tsx) |
| log:2 | refreshes its filters from the Fleet, and restores an entry's previous value as a new entry (log.test.tsx) |
| sync:1 | applies the reviewed sync plan only after confirming, and asks again whenever the plan changed (sync.test.tsx) |
| sync:2 | previews the instruction files a Harness would get, file by file (sync.test.tsx) |
| settings:1 | shows this Machine's details in order, copies diagnostics as text, and saves the preferred editor (settings.test.tsx) |
| settings:2 | says when no editor was found and looks again on request (settings.test.tsx) |
| settings:3 | keeps an active auth rejection in the diagnostics, with a timestamp reachable by keyboard (settings.test.tsx) |
| settings:4 | tucks Rejoin away while joined, and confirms the effective Machine id and copied Assignment before rejoining (settings.test.tsx) |
| settings:5 | opens Rejoin for a revoked Machine, joining with the admin token and typing the copied Machine when the Fleet can't load (settings.test.tsx) |
| settings:6 | labels each Harness's origin, tucks away presets not on this Machine, and validates a new Harness before previewing it (settings.test.tsx) |
| settings:7 | turns a Harness off after a preview, taking out what sk1ll placed or leaving it, and retries when Take out fails (settings.test.tsx) |
| settings:8 | asks for a fresh preview when machine.toml changed meanwhile, lists what failed after applying, and refuses an invalid change (settings.test.tsx) |
| settings:9 | still opens machine.toml when it can't be parsed (settings.test.tsx) |
| versions:1 | offers a newer release at startup with its notes, installs it, and says why an install failed (versions.test.tsx) |
| versions:2 | hides a skipped version for good (versions.test.tsx) |
| versions:3 | checks only on demand once the startup check is off, and says when this build can't update itself (versions.test.tsx) |
| versions:4 | says which side to upgrade when the Server speaks another protocol, and offers this app's update when it is the older one (versions.test.tsx) |
| versions:5 | offers to restart a Daemon older than the app, and hides an older Server once dismissed, across launches (versions.test.tsx) |
| contract | every_api_type_the_gui_consumes_matches_its_golden_json_and_round_trips (crates/sk1ll-gui/src-tauri/tests/api_contract.rs) plus `ui/src/apiContract.ts` under `pnpm tsc --noEmit` |

## App.test.tsx

- defaults standalone %s to Library and hides fleet screens -> shell:1
- creates the skills repo on first run and refreshes status -> setup:1
- does not request fleet or history during standalone background refresh -> shell:1
- keeps a dirty Library editor mounted across daemon failure and saves against the original fingerprint -> editing:1
- says which side to upgrade when the Server speaks another protocol -> versions:4
- offers to restart a Daemon older than the app, and hides a dismissed older Server -> versions:5

## AppUpdate.test.tsx

- checks at startup and offers the update with its notes -> versions:1
- keeps unsaved edits when the discard prompt is cancelled -> dropped: not reachable by a user. The editor is a modal dialog, so the header's Update button is inert while a file is dirty; the shared leave guard (`canLeaveFile`) is pinned by editing:1
- hides a skipped version -> versions:2
- skips the startup check when it is turned off, and checks on demand in Settings -> versions:3
- says when this build cannot update itself -> versions:3
- shows why an install failed -> versions:1

## CommandPalette.test.tsx

- restores previous focus on %s dismissal -> shell:3
- lets navigation focus the destination -> shell:3
- matches every query word in any order and selects a machine with the keyboard -> shell:3
- finds actions and announces an empty result when a query word is missing -> shell:3
- resolves screen hash %s to %s -> shell:2
- returns focus to the previous element after a background action -> shell:3

## Conflicts.test.tsx

- shows one empty state when both lists are empty -> conflicts:1
- hides the empty section when recovered is %s -> conflicts:1
- defaults diffs using the combined total of %s entries -> conflicts:1, conflicts:2
- waits for both lists before choosing the initial diff state -> dropped: a race between the two list requests; the mock answers both together, and the resulting diff state is pinned by conflicts:1 and conflicts:2
- confirms Overwrite before acting -> conflicts:1
- explains recovery and confirms permanent discard -> conflicts:1
- shows edited time only for Harness conflicts with numeric timestamp %s -> conflicts:1, conflicts:2

## CopyAssignment.test.tsx

- previews a mapped merge and confirms with both previewed registrations and revisions -> matrix:3
- reloads the target and requires a new preview in the same dialog after a conflict -> matrix:3

## Draft.test.tsx

- Draft confirmations > freezes Restore all items and requires review when a new item arrives -> draft:2
- Draft confirmations > invalidates Restore selected when content changes at the same paths -> draft:2
- Draft confirmations > revalidates Commit content and missing items and keeps the edited message on review -> draft:1
- Draft confirmations > invalidates review for binary edits whose textual diff is unchanged -> draft:2
- Draft confirmations > cannot reconfirm Commit when all selected items disappeared -> draft:6
- Draft confirmations > keeps review invalidated even if a later refresh restores the original content -> draft:2
- Draft confirmations > blocks confirmation and shows errors when a Draft refresh fails -> draft:1
- Draft confirmations > honours disabled in an already-open shared confirmation footer -> dropped: internal prop of the shared confirmation footer; disabled confirm buttons are asserted in draft:1, draft:2 and draft:6
- returns focus to %s after Cancel and Escape -> draft:1, draft:2
- returns focus to Commit after Cancel and Escape and to its completion notice after success -> draft:1
- counts added and removed lines without headers and shows friendly names -> draft:1
- pushes unpushed items -> draft:4
- selects arriving items until selection is changed by hand -> draft:2
- expands all diffs in the list and confirmation -> draft:1
- only offers Pull when behind or blocked and places blocked Pull in the notice -> draft:1, draft:3
- keeps one Push action after an unpushed commit and marks matching Draft items -> draft:4
- disables Push and Pull without an origin in standalone -> draft:6
- pushes separately after a standalone commit when selected -> draft:5
- lists what an interrupted Keep mine left in recovery, with the command for each -> draft:4
- offers Dismiss notes only when the report has notes -> draft:4
- shows no interrupted Keep mine notice without a report -> dropped: absence of a notice; every other Draft flow runs without a report
- Keep mine > confirms that the whole file wins, then shows what it kept and its diff -> draft:3
- Keep mine > keeps every clashing item at once, and says when the pull is still blocked -> draft:3
- Keep mine > offers Restore next to Keep mine to take the Baseline instead -> draft:3

## Fleet.test.tsx

- filters Machines by attention and connection, then clears the filter -> fleet:1
- describes every real status and the total in the fleet health bar -> fleet:1
- queues only this Machine’s work and navigates without changing state -> fleet:2
- offers Review Draft when only %s are reported -> dropped: wording variants of the Up next queue; its entries and actions are pinned by fleet:2
- shows All caught up for %s and ignores remote work -> fleet:2 (remote work); the "All caught up" wording is dropped
- preserves Machine details, local actions, timestamps, and removal confirmation -> fleet:1
- creates an Add Machine join code through the existing flow -> fleet:1
- shows each Machine's scope and lets a Self-only Machine remove only itself -> fleet:3
- handles an empty Fleet and a filter with no matches -> dropped: empty-state wording
- shows loading and initial errors without fabricating Fleet data -> dropped: initial-load error; the failure that could show stale data (a failed refresh) is pinned by fleet:1
- refreshes health and actions on revision and retains data when refresh fails -> fleet:1
- shows action counts with their purpose -> fleet:2
- opens with Ctrl+K and navigates to Library from App -> shell:3
- persists a collapsed sidebar through wide resizes and remounts -> dropped: window-width layout; the flows run at one desktop width
- auto-collapses on narrow windows without losing the expanded preference -> dropped: window-width layout, as above
- keeps a manual expansion on a narrow window until the breakpoint is crossed -> dropped: window-width layout, as above
- lists this Machine's work in Up next from the Daemon status when the Fleet omits it -> dropped: fallback data source for Up next; the queue itself is pinned by fleet:2
- keeps the sidebar usable when localStorage is unavailable -> dropped: storage-failure handling (try/catch around localStorage), implementation detail
- names the first conflict and opens Conflicts with heading focus -> fleet:2, conflicts:1
- push handles pushed=%s -> fleet:2 (pushed); the not-pushed toast wording is dropped
- counts queue entries, adopts in Library, and hides Push without unpushed items -> fleet:2
- renders local assignments, toggles a cell and opens the matrix -> fleet:2
- rolls a failed local assignment toggle back and shows the error -> fleet:2
- keeps a toggled row in place even when it drops below the six-row ranking -> dropped: row-ordering detail of the Fleet's assignment panel
- reloads the assignments panel when the machine identity changes -> dropped: needs a rejoin mid-session; every write carries `expected.registration`, pinned by matrix:1 and fleet:2
- does not claim the queue is empty while unmanaged-skill discovery fails or loads -> dropped: loading/failed wording of Up next
- keeps essential Machine details and hides empty work fields -> fleet:1
- names unpushed items and shows the assignment preview total -> fleet:2
- shows every Machine again when the selected Machine was removed -> dropped: selection reset after a removal; the removal is pinned by fleet:1
- leads Blocked guidance with Keep mine and keeps Restore as taking the Baseline -> fleet:1

## HarnessSettings.test.tsx

- labels origins and hides presets that are not on this Machine -> settings:6
- previews a disable and can leave what sk1ll placed in place -> settings:7
- takes out what sk1ll placed and turns the Harness off in one step -> settings:7
- leaves the Harness on when placements fail to come out, and retries -> settings:7
- says what failed when nothing is left but the scan or machine.toml stopped Take out -> dropped: a wording variant (no `left` count) of the not-taken-out notice pinned by settings:7
- offers Take out and remove for a removal, and only Apply when nothing is placed -> settings:8
- validates a new Harness before previewing it -> settings:6
- shows why a change is refused and does not apply it -> settings:8
- offers opt-in presets as available with Enable, and gemini its skills directory -> settings:6 (Enable); the "skills directory too" label variant is dropped
- asks for a fresh preview when machine.toml changed meanwhile, and lists what failed after applying -> settings:8
- asks for a fresh preview when machine.toml changed before Take out -> settings:8
- opens the daemon configuration even when settings cannot be parsed -> settings:9

## Imports.test.tsx

- prefills a single candidate and adds it into Draft -> imports:5
- passes a ref override and shows warnings without disabling add -> imports:3
- previews upstream changes and can %s after a refusal -> imports:1, imports:2
- leaves all candidates unchecked for a repository -> imports:3
- blocks taken, inventory, duplicate and invalid names -> imports:3
- adds sequentially, attempts every selection after a failure and retries only the unresolved ones -> imports:3
- reads back what landed when a failed response followed a successful write -> imports:4
- does not count an Import with another %s as landed -> imports:5
- still attempts the rest after the Daemon was unreachable for one -> imports:3
- leaves the rest not attempted when the batch is stopped -> imports:4
- checks names with the Daemon again before a retry and still adds the others -> imports:3
- confirms before a retry a Skill that landed although nothing could be read back -> imports:5 (the retry reads back first and adds once)
- asks to review the Draft when what landed cannot be read back -> imports:5
- rechecks name clashes against the refreshed inventory before a retry -> dropped: the retry's name re-check is pinned by imports:3; the "taken now" wording is dropped
- lets a deleted Draft item's name be reused -> imports:5
- changes the source while preserving Link and Path and clearing candidates -> dropped: form-state detail; imports:3 pins that the source can't change once something was added
- names the restored %s value and Assignment in success feedback -> log:2
- names the item, Harness and path in the %s recovery confirmation -> conflicts:1
- focuses resolved results and announces %s Skills found -> imports:3
- associates name errors with their input and clears the description after correction -> imports:3
- describes join errors from the submitted fields without losing existing hints -> setup:4
- focuses Review in Draft when adding finishes and announces only the latest success -> imports:3
- focuses the error summary when an import batch stops -> imports:3
- opens Add Import directly when requested from Skills -> shell:3 (from the palette)
- returns focus to Create join code on close -> fleet:1
- creates a Self-only join code and shows its exact expiry -> fleet:1 (Self-only code); the exact expiry wording is dropped
- tells a Self-only Machine it cannot create join codes -> fleet:3
- shows Imports the Server skipped, with the reason -> imports:1
- confirms Restore and guards repeated requests while pending -> log:2
- focuses the log region when the restored row is gone and empty is %s -> dropped: focus fallback when the restored row disappears
- lists all Imports, checks now and removes into Draft -> imports:1
- loads conflict diffs on expansion and uses recovery ids -> conflicts:2
- queries the server by machine, Harness, author and item -> log:1
- does not offer Restore for a removed Machine -> log:2
- updates all Imports sequentially and reports failures while continuing -> imports:2
- submits both Import steps through forms and keeps name failures free of repository advice -> imports:4 (repository advice on a resolve error); the form-submission detail is dropped
- adds repository guidance only for relevant resolve errors: %s -> imports:4
- reopens Add Import for each new request while the screen stays mounted -> dropped: remount detail of the request counter
- refreshes the upstream commit before another update after a conflict -> imports:1

## Log.test.tsx

- queries filters on the server and resets pagination -> log:1
- loads global facets and refreshes them with revision -> log:2
- shows entry IDs and readable causes -> log:1
- holds pagination until a typed search is applied, so a stale cursor is never reused -> log:1

## Matrix.test.tsx

- Assignment response cache > loads and refreshes assignments through GET without writing -> matrix:1
- Assignment response cache > keeps concurrent successful writes when B supersedes A refresh and an older snapshot arrives -> dropped: response-ordering cache; a write based on a stale snapshot is refused by its `expected` revision, pinned by matrix:1
- Assignment response cache > ignores a mutation response older than a snapshot already accepted for that machine -> dropped: response ordering, as above
- Assignment response cache > surfaces refresh failures while retaining the successful mutation response -> dropped: a refresh failing right after a write; load failures and their retry are pinned by matrix:6
- Assignment response cache > rolls back a failed item write and allows a retry -> matrix:6
- Assignment response cache > sends only the default change and renders materialized values from the Server response -> matrix:1
- Assignment response cache > labels unpushed Commits and Draft-only additions consistently -> matrix:1
- focuses and highlights the requested local cell after loading without changing its Assignment -> library:1
- a Machine whose Assignment fails to load > keeps the other Machines usable and retries only its own column -> matrix:6
- a Machine whose Assignment fails to load > keeps the error when a retry fails again -> matrix:6
- a Machine whose Assignment fails to load > sends bulk changes and their undo only for loaded Machines -> matrix:6
- a Machine whose Assignment fails to load > still shows a Machine without reported Harnesses so it can be retried -> dropped: the no-Harness variant of the failing column pinned by matrix:6
- filters by description and kind and shows friendly names -> matrix:2
- marks mixed states and batches row changes once per machine -> matrix:2
- persists hidden machines and limits column bulk changes to filtered items -> matrix:2
- resets row overrides optimistically and skips unavailable remote Draft items -> matrix:1, matrix:2
- describes explicit values that override the Harness default and clears the note on reset -> matrix:1
- marks offline Machines in the header and on their cells while keeping them editable -> matrix:1
- collapses a Machine to a summary column and remembers it -> matrix:2
- shows only this Machine or all Machines from the filter shortcuts -> matrix:2
- announces accepted bulk changes but not failed ones -> matrix:2
- undo restores explicit and inherited values only for accepted Machines -> matrix:2
- undo is refused by the authority when the Assignment changed meanwhile, then refreshes -> dropped: the same `expected`-revision refusal as matrix:1, reached through Undo
- drops a write reply for a registration that joined again within the same second -> dropped: registration race in the response cache; writes carry `expected.registration` (matrix:1)
- refuses an Assignment read for another registration than the Fleet listed -> dropped: registration race, as above
- assigns Draft-only items locally in standalone without requesting fleet -> matrix:7
- resets cached revisions and pending writes when authority becomes %j -> dropped: cache reset on a mode change mid-session
- scopes bulk $phase across a $field change -> dropped: filter change during a pending bulk change; the filter scoping itself is pinned by matrix:2
- shows a six-second joined load while five-second status polling continues -> dropped: loading-indicator timing
- Machine-indexed state > treats ids like constructor and toString as plain Machine ids -> dropped: object-key hygiene, implementation detail
- Machine-indexed state > forgets a Machine that left the Fleet, so coming back unloaded shows its error -> dropped: cache eviction detail
- Machine-indexed state > drops the snapshot of a Machine that joined again -> dropped: registration race, as above
- Machine-indexed state > lets a load sent after the cached snapshot win even with a lower revision -> dropped: response ordering, as above
- Machine-indexed state > undoes only Machines whose Assignment is still what the update left -> matrix:2
- Machine-indexed state > does not undo for a Machine that left the Fleet -> dropped: Undo after a removal mid-session; the Server refuses it by `expected` revision
- Machine-indexed state > refuses a bulk change when the Assignment changed since it was shown -> matrix:1 (same `expected` refusal, single toggle)
- refreshes after a single guarded edit conflicts -> matrix:1
- uses the same toggle when the item appears under a new name -> dropped: toggles are keyed by Item ID, which the mock and every write in matrix:1/matrix:2 send
- clears orphan toggles and refreshes the count -> matrix:4, matrix:5
- shows baseline and recreated Draft items with the same name as separate rows -> dropped: row identity by Item ID

## Settings.test.tsx

- collapses rejoin when joined -> settings:4
- expands rejoin when revoked -> settings:5
- saves editor changes -> settings:1
- orders sections and hides inactive diagnostic rows -> settings:1
- reveals the join code and confirms the effective default Machine id -> settings:4
- joins with the admin token instead of a join code -> settings:5
- falls back to a text field when Fleet loading fails -> settings:5
- shows an editor empty state and allows refreshing -> settings:2
- uses readable Harness values and preserves path formatting -> settings:1
- keeps active diagnostic details with a keyboard-reachable timestamp -> settings:3
- copies diagnostics as plain text -> settings:1

## Setup.test.tsx

- chooses Harnesses before anything is synced -> setup:1
- joins a Fleet, waits for the restart and turns everything on -> setup:2
- reports failures from turning everything on before leaving -> setup:2
- keeps the earlier failures when a retry fails before writing anything -> dropped: retry wording of the turn-on step; reporting what failed is pinned by setup:2
- says when no Harness is on instead of leaving as if everything was turned on -> dropped: wording for a Machine with no Harness
- keeps the wizard on Harnesses while a choice saves, then lets it continue past a failed save -> setup:1
- starts a new Fleet with the admin token and skips straight to the matrix -> setup:4
- skips the starting Assignment when it was copied from another Machine -> setup:3
- keeps the wizard and shows the error when joining fails -> setup:4
- starts the Daemon from the connection screen -> setup:screen
- explains a missing sk1ll command instead of offering to start the Daemon -> setup:screen
- shows the getting-started checklist after setup until it is dismissed -> setup:1
- reopens the checklist for Machines set up before -> dropped: the palette's "Show getting started"; the checklist itself is pinned by setup:1
- offers restart guidance on an API mismatch (active service: %s) -> setup:screen
- shows terminal restart instructions when the service manager query fails -> setup:screen
- offers a complete one-line start command for an installed launch agent -> setup:screen
- shows launchd commands on macOS -> setup:screen
- installs the launch agent from the connection screen on macOS -> setup:screen
- opens the selected Machine from the command palette -> shell:3
- names origin when a standalone push fails -> dropped: toast wording
- leaves the restart screen when the Daemon becomes unreachable after a mismatch -> setup:5
- closes a reopened checklist once its last step is done -> dropped: checklist step bookkeeping
- only completes the Fleet commit step once it is pushed and not Blocked -> dropped: checklist step bookkeeping
- leaves the standalone commit step to do on a fresh repo with only its starter commit -> setup:1
- treats adopting Unmanaged skills as optional -> dropped: checklist step bookkeeping

## Skills.test.tsx

- filters four Types and searches by name in one table -> library:1
- sorts Name and Type and opens rows using the keyboard -> library:1
- confirms Fork before opening the Import in the preferred editor -> library:2
- Adopts the selected variant before opening and passes its hash to browsing -> library:2
- offers plain Adopt without opening an editor -> library:2
- chooses a Terminal editor from the split button and refreshes preference -> library:2
- preserves the chosen editor through Fork confirmation -> library:2
- shows frontmatter separately and toggles Rendered and Raw without raw HTML -> library:1
- browses a Rule single file and keeps deleted items read-only -> library:1 (Rule file); the deleted-item read-only case is dropped
- shows binary and size notices and reloads on revision changes -> dropped: notice wording for binary and oversized files
- keeps editor errors visible after Fork without repeating the conversion -> dropped: error display after a failed editor launch
- stops after failed Adopt and blocks clashing names -> library:4 (clashing names); the failed-Adopt stop is dropped
- Harness chips navigate to the Assignment matrix -> library:1
- refreshes the selected item Type and closes a removed item -> library:2, library:4
- does not display the previous file when another file fails to load -> dropped: stale-preview guard on a read error; saves are tied to the path and fingerprint they read (editing:1)
- does not keep another variant preview when reading the new variant fails -> dropped: as above, for variants
- disables adoption while %s and explains the connection requirement -> library:3
- disables a pending adoption confirmation when the Server disconnects -> dropped: disconnect while the confirmation is open; library:3 pins adoption off while disconnected
- opens an Unmanaged Harness chip at its selected variant without navigating to Assignment -> library:2
- offers Add Import for an empty inventory and keeps filtered empty results distinct -> library:1
- keeps confirmation focus inside the detail dialog on Cancel and Escape and focuses Review after Adopt -> library:2
- explains missing editors -> dropped: wording when no editor exists (Settings' empty state is settings:2)
- checks collisions only in the %s namespace -> library:4
- shows the unmanaged handoff notice and switches between Unmanaged and all entries -> fleet:2
- explains an empty name and submits the creation form -> library:4
- opens the matrix without a Harness cell target when none are enabled -> dropped: matrix navigation without a target cell
- confirms deletion of a %s item and closes detail on success -> library:4
- hides rename and delete for %s entries -> library:1
- validates and submits rename for %s, selecting its new name -> library:4
- offers Resume adoption only for the variant matching an interrupted Adopt -> dropped: label variant of Adopt for a resumable variant
- keeps failed rename and delete errors inline -> library:4
- applies each handoff: a content handoff clears an earlier Type filter, and a create handoff opens the form -> shell:3, setup:1
- guards file navigation and saves with Ctrl+S, then offers Draft review -> editing:1
- shows the disk diff before overwriting and handles another concurrent edit -> editing:1

## SyncDialogs.test.tsx

- shows the proposed file and switches the diff when a file tab is selected -> sync:2
- lists the dry-run plan and applies only after confirmation -> sync:1
- requires a second confirmation when the daemon returns a changed plan -> sync:1
- shows completed placements and errors when a reviewed apply stops partway -> sync:1

## dev/mock.test.ts

- provides reviewed dev workflows and rejects unknown commands -> dropped: the mock is now the backend of every flow, which exercises each of these commands (copy preview/apply: matrix:3; resolve with ref and path: imports:3; upstream changes: imports:1; file edit round trip: editing:1; historical variant: library:2; harness preview/apply: settings:6; sync preview/apply: sync:1)
- lists orphans from the mock Assignment and prunes only the permitted scope -> matrix:4, matrix:5

## invalidation.test.ts

- background invalidation > routes draft changes to local content and imports, without fleet -> shell:4
- background invalidation > routes status changes without imports or fleet -> shell:4
- background invalidation > keeps fleet changes away from open files and imports -> shell:4
- background invalidation > refreshes every topic after daemon reconnection or loss -> shell:4
- background invalidation > refreshes fleet assignments when a local assignment is applied -> shell:4

## model.test.ts

- Machine status and badges > uses the glossary severity ordering even when every condition applies -> dropped: `headline()` is not used by the UI (the Daemon and Server compute statuses, `sk1ll_proto::headline`); the label order is pinned by fleet:1's health bar
- Machine status and badges > handles absent reports and a Baseline that has not been created -> dropped: same unused `headline()`
- Assignment cells > distinguishes explicit on and off from inherited state -> matrix:1
- Assignment cells > removes explicit toggles when following the default again -> matrix:1
- Assignment cells > leaves default materialization to the Server -> matrix:1
- revisioned Assignment snapshots > rejects lower revisions per machine and accepts newer snapshots -> dropped: response-ordering cache, as in Matrix.test.tsx
- revisioned Assignment snapshots > takes a snapshot requested after the cached one arrived, whatever its revision -> dropped: as above
- revisioned Assignment snapshots > keeps prototype names as ordinary Machine ids -> dropped: object-key hygiene
- revisioned Assignment snapshots > labels unpushed content even when the item also exists in Baseline -> matrix:1
- first-run setup > warns about plain HTTP except to this computer -> setup:2
- releases and version notices > orders releases by number, then pre-release -> versions:5
- releases and version notices > says which side a protocol mismatch should upgrade -> versions:4
- releases and version notices > warns about an older compatible Server and an older Daemon -> versions:5

## textDiff.test.ts

- returns nothing for identical text -> dropped: internal helper
- emits a hunk with context around a change -> editing:1 (Overwrite shows the disk edit's diff)
- marks files present on one side only -> dropped: internal helper; only the Unmanaged variant comparison diffs one-sided files, which no flow opens

## New: API contract

Not a replacement for a deleted test. `contract` pins the JSON of every local API type `ui/src/types.ts` mirrors
(sk1ll-daemon `client` types, sk1ll-proto types they re-export, and the GUI bridge's own: `CommandError`,
`ReviewDraftEntry`, `ConnectionEvent`, `EditorsView`, `ServiceInfo`, `AvailableUpdate`, `Progress`), every enum
variant at least once. `ServiceInfo`, `Manager` and `Started` (sk1ll-daemon, another phase's crate) only derive
`Serialize`, so they are compared with the golden file but not read back. `ConflictDiffRequest` is left out: the GUI
sends one of two shapes, and the Rust struct serializes both fields as null.
