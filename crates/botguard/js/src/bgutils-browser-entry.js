// Bundle entry for bgutils.browser.js: bgutils-js for the system webview
// (the helper process, see src/helper.rs). It only publishes the two classes
// the page script (helper-page.js) needs; that script removes the global
// again right away.
//
// Bundled by build.rs (js/build.mjs) on every cargo build that sees a change.

import { BotGuardClient } from "bgutils-js/botguard";
import { WebPoMinter } from "bgutils-js/webpo";

Object.defineProperty(globalThis, "__elizabeatsBgutils", {
  configurable: true,
  enumerable: false,
  value: { BotGuardClient, WebPoMinter },
});
