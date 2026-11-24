# iOS packaging and native launcher

The current app is the `HorizonGame` target: the `horizon-official` Rust executable plus
`HorizonLauncherKit`, the native Swift launcher. The authoritative build and artifact audit
steps are in [the iOS workflow](../../.github/workflows/ios.yml). It uploads the
`horizon-ios-game-<attempt>` artifact containing `Horizon-game-unsigned.ipa`.

Use the workflow’s `Build the game` and `Bundle it` steps together to reproduce the
current distributable. The old shell target is retained for diagnostics only.

## Targets and local builds

`HorizonGame` is the playable app. `HorizonLauncherKit` supplies its SwiftUI launcher.
The separate `Horizon` target is an old diagnostic shell that calls `horizon_probe()`;
its output is not proof that the game starts. `GameShell/main.m` is a placeholder
replaced by Cargo's executable before signing.

From the repository root, the game build uses:

```sh
rustup target add aarch64-apple-ios
cargo build --release --locked --target aarch64-apple-ios -p horizon-official \
  --no-default-features --features audio,horizon-extras
( cd packaging/ios && xcodegen generate )
HORIZON_GAME_PREBUILT=1 xcodebuild -project packaging/ios/HorizonIOS.xcodeproj \
  -target HorizonGame -configuration Release -sdk iphoneos -arch arm64 \
  CODE_SIGNING_ALLOWED=NO CODE_SIGN_IDENTITY= CODE_SIGNING_REQUIRED=NO \
  CONFIGURATION_BUILD_DIR="$PWD/build-ios" build
```

Use the workflow's stamping, packaging and audit steps before distributing an IPA. The
stamping step records the release version and source commit. The workflow supplies
`HORIZON_MSA_CLIENT_ID` when compiling Rust and writes the same ID to the app's Info.plist.
For a custom client ID, use it in both steps.

An unsigned IPA needs signing before installation. For a development install, open
`packaging/ios/HorizonIOS.xcodeproj`, select the **Horizon game** scheme and set a
signing team on the `HorizonGame` target. The source, assets and account configuration
used for that build determine what it can run; a compile alone is not a gameplay test.

The native launcher supports touch layouts, keyboard/pointer input and controllers.
Keep Cargo's desktop `controllers` feature disabled on iOS: the app uses Apple's
GameController APIs instead. The [simulator preview](Preview/README.md) runs the
launcher and its tests with a fake backend, without starting the Rust game.

### Landscape, which two places have to say

The game runs in landscape on a phone; the native launcher can use portrait. Both the bundle and the active view controller must permit the requested orientation:

- `GameInfo.plist` lists the orientations the game/launcher bundle can use. The native
  launcher and winit controller narrow that mask as ownership moves between them.
- `ValidOrientations::Landscape` on the winit window attributes (the startup and game window setup). Without it the plist alone does not hold: winit implements
  `supportedInterfaceOrientations` on its own view controller from its default
  (`LandscapeAndPortrait`, and it never reads the plist), and a phone asks the root view controller rather than
  the bundle, so the game would rotate into portrait under it.

Both landscape directions, not one: a handheld gets to be held either way, and the window size is the same in
each, so nothing in the render path has to care.

### Where the game looks, on the phone

- It works in `$HOME/Documents`, which on iOS is the app's data container (`entry.rs` `into_app_sandbox`). Saves
  land in `Documents/saves`, settings in `Documents/options.txt`, pack caches in `Documents/.horizon-cache`.
- To save launcher and game diagnostics from the device, open **Settings**, scroll to
  **Logs**, and choose **Export log file**. The launcher opens Files so you can save a copy.
  Developers can also use Xcode's **Devices and Simulators** window to download the app container
  and read `Library/Application Support/horizon/logs/latest.log`.
- `Options::read` looks for `horizon-resources.json` in three places, in this order: a `resources` folder
  **beside the executable**, which on a phone means inside the bundle; then `Documents/resources`, which is
  where an import unpacks; then `Documents/.local/horizon-client/26.3`, the desktop fallback, which the Files
  app neither shows nor lets you create. If the bundled files fail the prepared-file check and
  the imported copy passes it, startup uses the imported copy.
- The first stop is a developer's own: in Xcode, add your prepared `.local/horizon-client/26.3` to the
  **Horizon game** target as a folder reference with a path in bundle of `resources` (produce folders: keep
  their name or rename it there). It is fine to sign for yourself and it is the fastest debug loop, but a
  bundle built that way is not publishable -- see the next section -- which is why the game does not stop
  looking at the bundle.
- `UIFileSharingEnabled` in `GameInfo.plist` is what makes `Documents` visible to the Files app and to a cable
  at all, and is the reason the second stop is a folder a player can reach.

### Bringing the game files across

A published `.ipa` carries no assets: `horizon-resources.json` in a bundle is a refusal in
`.github/workflows/ios.yml`, and the bundle a phone installs is read-only while the game runs anyway. So the
native launcher can download a prepared archive from the configured source, accept a
custom download link, or import a ZIP through Files. Preparation of raw Java files
runs on a computer. Use **Reinstall game files** when the engine rejects an existing
installation. The shared game's recovery screen also supports the routes below.

On the computer, run `horizon-prepare --check` before packing. If it reports missing sounds,
run `horizon-prepare sounds` against the same prepared folder. Then create the ZIP with the desktop client:

```sh
horizon-client --pack-resources .local/horizon-client/26.3 --out Horizon-26.3.zip
```

The archive's one top folder is the directory you pointed at, which is exactly what compressing the folder by
hand produces: `crates/horizon-client/src/import.rs` reads both shapes, and refuses an archive that is none of
its three kinds before it writes a single file. Roughly 589 MB unpacked, so allow for the archive and the tree
both being on the phone at once.

**Over Wi-Fi, while the phone is asking.** A game that cannot start does not exit; it keeps its window, shows the
loader's own reason, and asks for its files at `http://<address>:<port>/` with six digits beside it
(`crates/horizon-client/src/game/interface/import_screen.rs`, drawn with the one atlas that exists before Mojang's files
do). Open that address on the computer, choose the archive, type the digits, and watch the bar on the phone. Or,
from a terminal:

```sh
curl --upload-file Horizon-26.3.zip "http://<address>:<port>/upload?name=Horizon-26.3&pin=<digits>"
```

`name` is what a world or a pack is filed under, so it is worth passing; the game files land in
`Documents/resources` whatever it says.

The listener is up only while that screen is up, it takes one archive at a time, and the digits -- never in the
page it serves -- are what stops anyone else on the network from writing into the app: after thirty wrong codes
the session answers nobody, right code included, and only a relaunch asks with new ones. When the archive lands,
the port is closed, the game files are in `Documents/resources`, and the loader starts again on them. A wrong
code answers `403` and writes nothing. If what arrived still is not enough to start the game, the screen comes
back with the new reason and asks again on a new port with new digits, so a bad archive is one more send away
rather than a dead end.

**By cable, with no network involved.** Copy the same `.zip` into the app's `Documents` folder -- Finder's
*Files* tab, or iTunes' *File Sharing*, or any manager that gets there -- and launch. `main.rs` unpacks what it
finds there before the game looks for its resources, then deletes the archive.

ZIP imports accept up to 2 GiB compressed. Worlds and prepared game files must also fit within
4 GiB unpacked. For larger world backups, extract the ZIP on a computer and copy the folder
containing `level.dat` or `level.dat_old` into `Documents/saves` under a new name.

Both doors route by the archive's root, so one file format carries three things: `horizon-resources.json` is
the game files, `level.dat` or `level.dat_old` is a world (into `Documents/saves`, never over one that exists), `pack.mcmeta` is a
resource pack, which stays a `.zip` because the game reads those where they are. An archive that will not
unpack is renamed `<name>.zip.failed` rather than retried every launch, and an unpacking that a flat battery
interrupts is finished, or its old tree put back, on the next start -- `import::recover`, before anything reads
the folder it was working on.

What is measured of this, on Linux rather than on a phone: a game started with no files stayed open, refused a
wrong code with `403` and wrote nothing, took a real archive posted to the address it logged, unpacked it into
`Documents/resources`, closed the port and started the loader again -- then asked again on a fresh port and code
when that archive turned out to be a three-file stub. `HORIZON_IMPORT_SCREEN=1` is the same decision the iOS
build makes on its own, which is how that run was possible without a phone.

What is left to see on a device: the screen's text is drawn from `horizon-theme`'s embedded font, so it needs
nothing installed, but nobody has read it on a real phone yet. Nobody has run the game on a device either; what
is measured there is in the workflow's summary -- the game compiles for the target, links, and comes out of
Xcode's bundle as the same bytes Cargo wrote.

### Checking first-run import without a device

The shared importer can be checked end to end with prebuilt desktop binaries and
resources prepared from your own game:

```sh
python3 tools/test_import_startup.py --bin-dir target/fast \
  --resources .local/horizon-client/26.3
```

This starts a scratch game without resources, serves a temporary archive over
loopback, and requires the game to reach its title without restarting and exit
cleanly. It checks the installed manifest and rejects Authorization headers on the
automatic download. Logs and a JSON result go under `.local/import-startup`; the
archive and installed test copy are removed afterward. `--scratch-parent` can put
those temporary files on a drive with enough free space.

The desktop explicitly enables `HORIZON_IMPORT_SCREEN` to exercise the shared
first-run flow. This does not verify physical iOS input, orientation, or shader
appearance. Native launcher download/import presentation is covered separately by
`tools/test_ios_game_files.py` on the Simulator.

## Why the app bundles a Rust executable

Gameplay and startup now live in the `horizon-client` library (`src/lib.rs`, `game.rs`,
`src/game/`). Its executable is a thin entry point; `horizon-official` provides the shipped
Horizon app entry. The earlier claim that gameplay existed only in a binary target is obsolete.

The iOS build still packages a Cargo executable because winit’s UIKit backend starts the
application event loop. Xcode bundles that executable with `HorizonLauncherKit` and the app’s
resources, and `tools/ci/ios_release.py` stamps and audits the final app and IPA.

### Unsigned builds in GitHub Releases

A successful trusted `main` push publishes a regular release containing an unsigned IPA
and its `.sha256` checksum. `tools/ci/publish-ios-release.sh` explicitly clears the
prerelease flag and only updates a tag whose source commit matches the build.
The app's build number is `<run>.<attempt>`. Its source commit is recorded in
`Info.plist`. Release tags use `ios-build-<run>.<attempt>` and assets use
`Horizon-iOS-<version>-build<run>.<attempt>-unsigned.ipa`.

PRs and fork builds only upload temporary Actions artifacts. A manual run on `main`
can publish by selecting **publish_release**; other manual runs remain artifact-only.
The publishing job receives `contents: write` and retrieves the successful build's
artifact by ID. The audit verifies its source commit, launcher framework, client ID,
and configured minimum iOS version. It also checks each bundled Mach-O for the iOS
device target and absence of signatures, and rejects signing material, private files,
and prepared game data.

The job verifies the GitHub asset digests for the IPA and its checksum before making
a draft public. A release-only retry can resume a partial upload for the same build.
The release job compares an existing tag to the build commit before uploading and
again before publishing. It marks only the newest eligible release as `latest`.
Downloaded IPAs need signing before installation.
