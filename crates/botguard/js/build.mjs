// Builds the two browser bundles injected into the webview helper page.
// Run by the crate's build.rs as `node build.mjs <output dir>`; the output
// lives in Cargo's build directory, never in the repo.
import { build } from "esbuild";

const outDir = process.argv[2];
if (!outDir) {
  console.error("usage: node build.mjs <output dir>");
  process.exit(2);
}

// ES2020 runs on every WebKit / WebView2 / WebKitGTK version wry supports.
const common = {
  bundle: true,
  format: "iife",
  platform: "browser",
  target: "es2020",
  charset: "utf8",
  logLevel: "warning",
};

await build({
  ...common,
  entryPoints: ["src/youtube-browser-entry.js"],
  outfile: `${outDir}/youtube.browser.js`,
  // ~0.7 MB minified instead of ~1.4 MB; licenses kept at the end.
  minify: true,
  legalComments: "eof",
});

await build({
  ...common,
  entryPoints: ["src/bgutils-browser-entry.js"],
  outfile: `${outDir}/bgutils.browser.js`,
  // Small, so kept readable.
  legalComments: "inline",
});
