// Bundle entry for youtube.browser.js: youtubei.js for the system webview
// (the helper process, see src/helper.rs). It only publishes what the page
// script (helper-page.js) needs; that script removes the global again right
// away.
//
// Bundled by build.rs (js/build.mjs) on every cargo build that sees a change.

// The web entry matters: it fixes the module evaluation order of
// youtubei.js's circular imports and installs the browser platform shim.
import { Innertube, Log, Platform } from "youtubei.js/web";

// Nobody reads the webview's console; the page reports errors itself.
Log.setLevel(Log.Level.NONE);

Object.defineProperty(globalThis, "__elizabeatsYoutube", {
  configurable: true,
  enumerable: false,
  value: { Innertube, Platform },
});
