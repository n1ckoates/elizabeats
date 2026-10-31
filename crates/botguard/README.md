# elizabeats-botguard

YouTube "proof of origin" (PO) tokens and audio stream URLs for Elizabeats.

Both happen in the operating system's own browser engine, in a helper
process: the app starts its own executable with `--elizabeats-webview-minter`,
which opens an invisible window with a webview on
`https://www.youtube.com/robots.txt`. YouTube rejects tokens minted in fake
browser environments (QuickJS, jsdom), so a real engine is required.

- **Minting** runs bgutils-js in that page (BotGuard challenge, integrity
  token, `WebPoMinter`).
- **Stream resolution** runs youtubei.js in the same page: it mints the token
  for the video ID, asks the YouTube Music client for the formats, picks the
  best AAC/MP4 audio-only one and deciphers its URL. The page's own `fetch`
  is used; YouTube's CORS rules allow the calls to music.youtube.com, the
  iframe API and the player script from www.youtube.com. The URL downloads
  with plain ranged GETs (`&range=start-end`, at most 10 MB each) using the
  webview's User-Agent, which is returned with the stream.

The page keeps the minter and the youtubei.js session warm, recreates them
(retrying once) when a warm one fails, and applies its own timeouts. The app
talks to the helper with newline-delimited JSON over stdin/stdout (see
`src/protocol.rs`: `{"id":1,"identifiers":[…]}` mints, `{"id":2,"resolve":"<videoId>"}`
resolves), starts it on first use, kills and restarts it when it hangs or
crashes, and the helper exits as soon as its stdin closes.

Typical timings (macOS): about 1 s for the first resolve (helper start,
minter, youtubei.js session), 0.2-0.3 s afterwards.

The app must call `elizabeats_botguard::helper_main_if_requested()` first thing
in `main` (before any UI) and exit with the returned code when it is `Some`.

## Platform requirements

| OS      | Webview                                  | Notes |
|---------|------------------------------------------|-------|
| macOS   | WKWebView (built in)                     | Helper runs without a Dock icon and never takes focus. |
| Windows | WebView2 (built into Windows 10/11)      | Without the WebView2 runtime minting fails with an error. |
| Linux   | WebKitGTK 4.1 via wry's GTK backend      | See below. |

**Linux:** the app links against WebKitGTK, so

- to **run**, `webkit2gtk-4.1` must be installed (e.g. Debian/Ubuntu
  `libwebkit2gtk-4.1-0`, Fedora `webkit2gtk4.1`, Arch `webkit2gtk-4.1`).
  It is a dynamic library dependency of the whole executable, not just of the
  helper: without it the app does not start at all. A display (X11 or
  Wayland) is needed for the helper; without one minting fails with an error.
- to **build**, the development packages are needed (e.g. Debian/Ubuntu
  `libwebkit2gtk-4.1-dev libgtk-3-dev`, Fedora `webkit2gtk4.1-devel`).

## JavaScript

The bundles are built at compile time: `build.rs` runs `pnpm install` (only when
`js/pnpm-lock.yaml` changed) and `node js/build.mjs` into Cargo's output folder,
and `src/helper.rs` embeds them from there. Building therefore needs **Node 22+
and pnpm**; nothing generated is committed.
All three scripts are injected into the helper page at document start:

- `bgutils.browser.js` (built): bgutils-js, from `js/src/bgutils-browser-entry.js`.
- `youtube.browser.js` (built): youtubei.js, browser build (from
  `js/src/youtube-browser-entry.js`).
- `js/src/helper-page.js`: the page logic (minting, resolving, warm state);
  injected as it is.

## Tests

```sh
cargo test -p elizabeats-botguard                      # offline: protocol, fake helper
cargo test -p elizabeats-botguard --test live -- --ignored --nocapture --test-threads=1
```

The live tests use YouTube and the real webview; the helper executable is the
test binary `elizabeats-botguard-helper`, which also has scripted fake modes
(`--fake-minter <mode>`) for the offline process tests.
