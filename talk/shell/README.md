# Talk Viewer

A macOS presentation window with a real WebKit browser and the live display of
an iPad connected over USB. The divider is draggable and the window supports
native macOS full-screen mode.

The iPad pane requests the largest format the device advertises. Pinch to zoom
from fit-to-pane up to 8×, then use a trackpad, mouse wheel, or the native scroll
bars to pan across the display. The header also has **−**, **Fit**, and **+**
controls; the matching shortcuts are Command-Option-Minus, Command-Option-0,
and Command-Option-Plus.

## Requirements

- macOS 13 or newer
- Xcode or the current Xcode Command Line Tools (`xcode-select --install`)
- An unlocked iPad that has trusted the Mac
- A data-capable USB cable

The app uses the muxed external capture stream that macOS exposes for trusted
iOS and iPadOS devices. It does not select a Continuity Camera or a Mac webcam.

## Build and run

From the repository root on a Mac:

```bash
direnv allow
direnv exec . ./talk/shell/build-app
open "talk/shell/dist/Talk Viewer.app"
```

Run the Swift unit tests with:

```bash
./talk/shell/build-app --test
```

### Swift toolchain setup

If the build reports `tool 'swift' not found`, install Apple's command-line
toolchain:

```bash
xcode-select --install
xcrun --find swift
```

If Xcode is already installed, make it the active developer directory instead:

```bash
sudo xcode-select --switch /Applications/Xcode.app/Contents/Developer
sudo xcodebuild -runFirstLaunch
xcrun swift --version
```

After a macOS upgrade, Software Update may need to install a matching Command
Line Tools release before `xcrun` can find Swift again.

The build script deliberately removes Nix compiler and SDK variables before it
runs Swift, then derives `MacOSX.sdk` directly from the selected Swift binary's
Xcode or CommandLineTools directory. This keeps an active Xcode or Xcode-beta
compiler paired with the SDK shipped in that same installation; mixing it with
the Nix-provided macOS SDK produces `no such module 'SwiftShims'` and
SDK-version errors.

The browser opens `http://127.0.0.1:10000` by default, which is the standard
blit development UI address. Start at another page by passing arguments through
`open`:

```bash
open "talk/shell/dist/Talk Viewer.app" --args \
  --url http://localhost:3000 \
  --device "Alice's iPad"
```

The first non-option argument can also be the URL. `TALK_VIEWER_URL` and
`TALK_VIEWER_DEVICE` provide environment-variable equivalents when launching
the executable directly.

On first launch, allow camera access. macOS places the iPad's display feed under
that privacy category. If no display appears:

1. Unlock the iPad, reconnect it, and accept **Trust This Computer**.
2. Quit QuickTime Player, OBS, or another app that may already own the feed.
3. Check **System Settings → Privacy & Security → Camera → Talk Viewer**.
4. Press the refresh button beside the device picker.

The USB feed is video-only: Talk Viewer neither records nor forwards the iPad's
audio.
