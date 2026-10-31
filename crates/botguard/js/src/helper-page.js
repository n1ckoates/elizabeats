// The page script of the webview helper process (src/helper.rs): mints PO
// tokens and resolves audio stream URLs with youtubei.js, all inside the
// system webview.
//
// Injected (after the bgutils and youtubei.js bundles that build.rs makes) into
// https://www.youtube.com/robots.txt. BotGuard requests are same-origin;
// youtubei.js's requests to music.youtube.com, the iframe API and the player
// script are allowed by YouTube's CORS rules, so the page's own fetch works.
// Not bundled: the file is embedded into the Rust crate as it is.
//
// Rust → page:
//   window.__elizabeatsMint(id, identifiers)   mint one token per identifier
//   window.__elizabeatsResolve(id, videoId)    resolve an AAC audio stream
// Page → Rust:  window.ipc.postMessage(JSON) with one of
//   {"type":"ready"}                                     page is set up
//   {"type":"reply","id":1,"tokens":[…],"valid_until":…}  tokens (unix seconds)
//   {"type":"reply","id":1,"stream":{url,mime_type,content_length,bitrate,itag,user_agent}}
//   {"type":"reply","id":1,"error":"…","user_facing":false}  request failed
//   {"type":"log","message":"…"}                          diagnostics
// An empty identifier list prewarms: it makes sure a minter exists (and is
// answered once it does) and starts the youtubei.js session in the background.
// `user_facing` errors are YouTube's own readable messages ("This video is
// unavailable", …) that the app may show as they are.

(() => {
  "use strict";
  if (window.top !== window || location.origin !== "https://www.youtube.com") {
    return;
  }
  const bgutils = globalThis.__elizabeatsBgutils;
  const youtube = globalThis.__elizabeatsYoutube;
  delete globalThis.__elizabeatsBgutils;
  delete globalThis.__elizabeatsYoutube;
  if (!bgutils) {
    return;
  }

  const REQUEST_KEY = "O43z0dpjhgX20SCx4KAo";
  const GOOG_API_KEY = "AIzaSyDyT5W0Jh49F30Pqqtyfdf7pDLFKLJoAnw";
  const CLIENT = { clientName: "MWEB", clientVersion: "2.20260205.04.01", hl: "en", gl: "US" };
  const INTERPRETER_HOSTS = ["www.google.com", "google.com", "www.youtube.com", "youtube.com"];
  const CREATE_TIMEOUT_MS = 20000;
  const MINT_TIMEOUT_MS = 10000;
  // Replace the minter this long before YouTube says its integrity token expires.
  const EXPIRY_MARGIN_SECS = 60;
  // Creating a youtubei.js session downloads and analyses YouTube's player script.
  const SESSION_TIMEOUT_MS = 25000;
  // YouTube rotates its player script every few days; reload it periodically.
  const SESSION_MAX_AGE_MS = 6 * 60 * 60 * 1000;
  // The player request plus deciphering the URL, for one video.
  const PLAYER_TIMEOUT_MS = 20000;
  // A whole resolve request, retry included. Below the app's own limit
  // (RESOLVE_TIMEOUT, 60 s), so the page answers before the app gives up.
  const RESOLVE_BUDGET_MS = 50000;
  // Only retry when at least this much of the budget is left.
  const MIN_RETRY_MS = 5000;
  const UNSUPPORTED_FORMAT = "YouTube returned an unsupported audio format.";

  const post = (message) => window.ipc.postMessage(JSON.stringify(message));
  const log = (message) => post({ type: "log", message: String(message) });
  const describe = (error) => String((error && error.message) || error || "unknown error");

  function withTimeout(promise, ms, what) {
    let timer;
    const timeout = new Promise((_, reject) => {
      timer = setTimeout(() => reject(new Error(`${what} timed out`)), Math.max(0, ms));
    });
    return Promise.race([promise, timeout]).finally(() => clearTimeout(timer));
  }

  /** A message from YouTube that can be shown to the user as it is. */
  class UserFacingError extends Error {}

  // ---------------------------------------------------------------- minting

  async function postJson(path, headers, body) {
    const response = await fetch(path, { method: "POST", headers, body: JSON.stringify(body) });
    if (!response.ok) {
      throw new Error(`${path.split("?")[0]} answered HTTP ${response.status}`);
    }
    return response.json();
  }

  /** Loads Google's BotGuard interpreter with a real <script> tag. */
  function loadInterpreter(challenge) {
    const raw = challenge.interpreterUrl?.privateDoNotAccessOrElseTrustedResourceUrlWrappedValue;
    if (!raw) {
      throw new Error("the BotGuard challenge has no interpreter URL");
    }
    const url = new URL(raw.startsWith("//") ? `https:${raw}` : raw);
    if (url.protocol !== "https:" || !INTERPRETER_HOSTS.includes(url.hostname)) {
      throw new Error(`untrusted BotGuard interpreter URL ${url.hostname}`);
    }
    return new Promise((resolve, reject) => {
      const script = document.createElement("script");
      script.src = url.href;
      script.onload = () => {
        script.remove();
        resolve();
      };
      script.onerror = () => {
        script.remove();
        reject(new Error("could not load the BotGuard interpreter"));
      };
      (document.head || document.documentElement).appendChild(script);
    });
  }

  /** Runs the BotGuard challenge and returns a fresh WebPoMinter. */
  async function createMinter() {
    const started = performance.now();
    const attestation = await postJson(
      "/youtubei/v1/att/get?prettyPrint=false",
      { "Content-Type": "application/json" },
      { context: { client: CLIENT }, engagementType: "ENGAGEMENT_TYPE_UNBOUND" }
    );
    const challenge = attestation && attestation.bgChallenge;
    if (!challenge || !challenge.program || !challenge.globalName) {
      throw new Error("YouTube sent no BotGuard challenge");
    }
    await loadInterpreter(challenge);
    const client = await bgutils.BotGuardClient.create({
      globalName: challenge.globalName,
      globalObject: window,
      program: challenge.program,
    });
    const webPoSignalOutput = [];
    const botguardResponse = await client.snapshot({ webPoSignalOutput });
    const it = await postJson(
      "/api/jnn/v1/GenerateIT",
      {
        "Content-Type": "application/json+protobuf",
        "x-goog-api-key": GOOG_API_KEY,
        "x-user-agent": "grpc-web-javascript/0.1",
      },
      [REQUEST_KEY, botguardResponse]
    );
    if (!Array.isArray(it) || typeof it[0] !== "string" || !it[0]) {
      throw new Error("YouTube rejected the BotGuard attestation (no integrity token)");
    }
    const ttl = Number(it[1]) > 0 ? Number(it[1]) : 3600;
    const threshold = Number(it[2]) > 0 ? Number(it[2]) : 0;
    const minter = await bgutils.WebPoMinter.create(
      {
        integrityToken: it[0],
        estimatedTtlSecs: ttl,
        mintRefreshThreshold: threshold,
        websafeFallbackToken: typeof it[3] === "string" ? it[3] : undefined,
      },
      webPoSignalOutput
    );
    const usableSecs = Math.max(ttl - Math.max(EXPIRY_MARGIN_SECS, threshold), ttl / 2);
    const now = Date.now();
    log(`created WebPoMinter in ${Math.round(performance.now() - started)} ms (TTL ${ttl} s)`);
    return { minter, createdAt: now, expiresAt: now + usableSecs * 1000 };
  }

  let currentMinter = null; // { minter, createdAt, expiresAt }
  let creatingMinter = null; // promise while a minter is being created

  /** The warm minter, or a new one when there is none or it expired. */
  function getMinter() {
    if (currentMinter && Date.now() < currentMinter.expiresAt) {
      return Promise.resolve(currentMinter);
    }
    currentMinter = null;
    if (!creatingMinter) {
      creatingMinter = withTimeout(createMinter(), CREATE_TIMEOUT_MS, "creating the BotGuard minter")
        .then((entry) => {
          currentMinter = entry;
          return entry;
        })
        .finally(() => {
          creatingMinter = null;
        });
    }
    return creatingMinter;
  }

  function forgetMinter(entry) {
    if (entry && currentMinter === entry) {
      currentMinter = null;
    }
  }

  async function mintAll(entry, identifiers) {
    const tokens = [];
    for (const identifier of identifiers) {
      const token = await withTimeout(
        entry.minter.mintAsWebsafeString(identifier),
        MINT_TIMEOUT_MS,
        "minting a PO token"
      );
      if (typeof token !== "string" || token.length < 40) {
        throw new Error("BotGuard produced an implausible PO token");
      }
      tokens.push(token);
    }
    return tokens;
  }

  async function mint(id, identifiers) {
    if (identifiers.length === 0) {
      // Prewarm: get the youtubei.js session going too.
      getSession().catch((error) => log(`could not prewarm youtubei.js: ${describe(error)}`));
    }
    const requested = Date.now();
    let entry = null;
    try {
      entry = await getMinter();
      let tokens;
      try {
        tokens = await mintAll(entry, identifiers);
      } catch (error) {
        // A minter from an earlier request may have gone stale: retry once
        // with a fresh one. A brand-new minter failing is a real error.
        if (entry.createdAt >= requested) {
          throw error;
        }
        log(`minting with the cached minter failed (${describe(error)}); recreating it`);
        forgetMinter(entry);
        entry = await getMinter();
        tokens = await mintAll(entry, identifiers);
      }
      post({ type: "reply", id, tokens, valid_until: Math.floor(entry.expiresAt / 1000) });
    } catch (error) {
      forgetMinter(entry);
      post({ type: "reply", id, error: describe(error) });
    }
  }

  // ------------------------------------------------------------- youtubei.js

  if (youtube) {
    // youtubei.js runs YouTube's player code (for deciphering) through this.
    youtube.Platform.shim.eval = (data) => new Function(data.output)();
  }

  let session = null; // { yt, createdAt, refreshAt }
  let creatingSession = null;

  async function createSession() {
    if (!youtube) {
      throw new Error("youtubei.js is not available in the webview");
    }
    const started = performance.now();
    const yt = await youtube.Innertube.create({
      generate_session_locally: true,
      lang: "en",
      location: "US",
      // Must be bound to window: WebKit refuses `fetch` called on anything else.
      fetch: (...args) => window.fetch(...args),
    });
    if (!yt.session.player) {
      throw new Error("YouTube's player is unavailable");
    }
    const now = Date.now();
    log(`created youtubei.js session in ${Math.round(performance.now() - started)} ms`);
    return { yt, createdAt: now, refreshAt: now + SESSION_MAX_AGE_MS };
  }

  /** The warm youtubei.js session, or a new one. */
  function getSession() {
    if (session && Date.now() < session.refreshAt) {
      return Promise.resolve(session);
    }
    session = null;
    if (!creatingSession) {
      creatingSession = withTimeout(createSession(), SESSION_TIMEOUT_MS, "creating the youtubei.js session")
        .then((entry) => {
          session = entry;
          return entry;
        })
        .finally(() => {
          creatingSession = null;
        });
    }
    return creatingSession;
  }

  function forgetSession(entry) {
    if (entry && session === entry) {
      session = null;
    }
  }

  // The player's `po_token` is shared state, so player requests run one at
  // a time: each request sets its token, fetches and deciphers.
  let playerQueue = Promise.resolve();
  function exclusive(task) {
    const run = playerQueue.then(task);
    playerQueue = run.catch(() => {});
    return run;
  }

  /** The best AAC audio-only format: original track, non-DRC, then bitrate. */
  function pickAudioFormat(info) {
    const formats = info.streaming_data?.adaptive_formats ?? [];
    const candidates = formats.filter(
      (format) =>
        format.has_audio &&
        !format.has_video &&
        String(format.mime_type).toLowerCase().startsWith("audio/mp4") &&
        (format.url || format.signature_cipher || format.cipher)
    );
    const score = (format) => [
      format.audio_track && !format.audio_track.audio_is_default ? 0 : 1,
      format.is_drc ? 0 : 1,
      format.bitrate ?? 0,
    ];
    candidates.sort((a, b) => {
      const [sa, sb] = [score(a), score(b)];
      for (let i = 0; i < sa.length; i++) {
        if (sa[i] !== sb[i]) {
          return sb[i] - sa[i];
        }
      }
      return 0;
    });
    return candidates[0];
  }

  /** Player request + decipher for one video with one session. */
  async function fetchStream(entry, videoId, poToken) {
    const { player } = entry.yt.session;
    player.po_token = poToken;
    // The YouTube Music client is the one that still serves full songs over
    // plain ranged URLs.
    let info;
    try {
      info = await entry.yt.getBasicInfo(videoId, { client: "YTMUSIC" });
    } catch (error) {
      // youtubei.js throws for unplayable videos, with YouTube's
      // playability status (and its readable reason) attached.
      const status = error?.info?.status;
      if (typeof status === "string" && status !== "OK") {
        throw new UserFacingError(error.info.reason || "This video is unavailable.");
      }
      throw error;
    }
    const playability = info.playability_status;
    if (playability?.status && playability.status !== "OK") {
      throw new UserFacingError(playability.reason || "This video is unavailable.");
    }
    const format = pickAudioFormat(info);
    if (!format) {
      throw new UserFacingError(UNSUPPORTED_FORMAT);
    }
    const url = await format.decipher(player);
    if (typeof url !== "string" || !url.startsWith("https://")) {
      throw new Error("youtubei.js produced no usable stream URL");
    }
    const length = Number(format.content_length);
    return {
      url,
      mime_type: String(format.mime_type),
      content_length: Number.isFinite(length) && length > 0 ? length : null,
      bitrate: Number.isFinite(format.bitrate) ? format.bitrate : null,
      itag: format.itag,
      // Downloads must use the same User-Agent as the webview.
      user_agent: navigator.userAgent,
    };
  }

  /**
   * One attempt: mint the token (while the session loads), then fetch.
   * A failure is tagged `retryable` when it came from a minter or session
   * that was already warm before this attempt (it may have gone stale);
   * that minter or session is dropped so the next attempt makes a new one.
   */
  async function resolveOnce(videoId) {
    const attemptStarted = Date.now();
    const sessionPromise = getSession();
    sessionPromise.catch(() => {}); // awaited below
    const fail = (error, staleEntry) => {
      const wrapped = error instanceof Error ? error : new Error(describe(error));
      wrapped.retryable = Boolean(staleEntry && staleEntry.createdAt < attemptStarted);
      return wrapped;
    };

    let minterEntry = null;
    let token;
    try {
      minterEntry = await getMinter();
      [token] = await mintAll(minterEntry, [videoId]);
    } catch (error) {
      forgetMinter(minterEntry);
      throw fail(error, minterEntry);
    }

    const entry = await sessionPromise; // a failed creation is a plain error
    try {
      return await exclusive(() =>
        withTimeout(fetchStream(entry, videoId, token), PLAYER_TIMEOUT_MS, "the YouTube player request")
      );
    } catch (error) {
      if (error instanceof UserFacingError) {
        throw error; // the video is the problem, not the session
      }
      forgetSession(entry);
      throw fail(error, entry);
    }
  }

  async function resolve(id, videoId) {
    const deadline = Date.now() + RESOLVE_BUDGET_MS;
    const attempt = () =>
      withTimeout(resolveOnce(videoId), deadline - Date.now(), "resolving the audio stream");
    try {
      let stream;
      try {
        stream = await attempt();
      } catch (error) {
        if (!error?.retryable || deadline - Date.now() < MIN_RETRY_MS) {
          throw error;
        }
        log(`resolving ${videoId} with the warm minter/session failed (${describe(error)}); retrying`);
        stream = await attempt();
      }
      post({ type: "reply", id, stream });
    } catch (error) {
      post({
        type: "reply",
        id,
        error: describe(error),
        user_facing: error instanceof UserFacingError,
      });
    }
  }

  // ------------------------------------------------------------ entry points

  const define = (name, value) =>
    Object.defineProperty(window, name, { configurable: false, enumerable: false, value });
  define("__elizabeatsMint", (id, identifiers) => {
    void mint(id, identifiers);
  });
  define("__elizabeatsResolve", (id, videoId) => {
    void resolve(id, videoId);
  });
  window.addEventListener("unhandledrejection", (event) => log(`unhandled rejection: ${describe(event.reason)}`));

  const ready = () => post({ type: "ready" });
  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", ready, { once: true });
  } else {
    ready();
  }
})();
