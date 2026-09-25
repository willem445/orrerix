// The app's webview must not be throttled while its window is occluded or the
// display is off (#1141).
//
// Delivery itself does not run on the webview: the queue drainer, the paste,
// echo verification, Enter and the hold re-checks are backend threads reading
// the backend's own terminal grid (docs/design/webview-throttling.md). What
// DOES run there is xterm's parser, and it is the only thing that answers an
// agent CLI's terminal queries (DA, DSR/CPR, OSC colour, focus reports) — a
// reply is emitted only once xterm PARSES the query, on `setTimeout` chains
// Chromium clamps hard for a hidden page (after ~5 min, one wake-up a minute:
// Intensive Wake-Up Throttling). A CLI waiting on that reply stops taking
// input, so a delivery lands in its box and is never submitted until someone
// wakes the display. The fix is WebView2 browser arguments; this file pins
// them, because JSON carries no comment to stop a later edit dropping one.
//
// Three properties, each a way the config can be wrong while still parsing:
//   1. every throttling switch is present;
//   2. `additionalBrowserArgs` REPLACES wry's own default argument string
//      rather than appending to it (wry 0.55 `src/webview2/mod.rs`,
//      `create_environment`: `additional_browser_args.unwrap_or_else(|| …
//      default_args …)`), so wry's three disabled features must be restated
//      here or they silently come back on;
//   3. there is exactly ONE `--disable-features=` switch. Chromium keeps the
//      last occurrence of a repeated switch, so a second one would drop
//      whichever list came first — wry's defaults or the throttling feature —
//      with nothing to say so.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

const CONF = fileURLToPath(new URL("../src-tauri/tauri.conf.json", import.meta.url));

/** wry's default WebView2 argument string's features, restated in the config. */
const WRY_DEFAULT_DISABLED_FEATURES = ["msWebOOUI", "msPdfOOUI", "msSmartScreenProtection"];

/** The Chromium feature that clamps a hidden page's chained timers to one
 *  wake-up a minute after five minutes hidden. */
const THROTTLING_FEATURES = ["IntensiveWakeUpThrottling"];

/** Chromium switches that stop an occluded/background renderer being throttled
 *  or deprioritised. */
const THROTTLING_SWITCHES = [
  "--disable-background-timer-throttling",
  "--disable-backgrounding-occluded-windows",
  "--disable-renderer-backgrounding",
];

type WindowConf = { additionalBrowserArgs?: string };

function windows(): WindowConf[] {
  const conf = JSON.parse(readFileSync(CONF, "utf8")) as { app: { windows: WindowConf[] } };
  return conf.app.windows;
}

/** Split an argument string on whitespace (WebView2 takes a command-line
 *  fragment; none of these values carries a quoted space). */
function tokens(args: string): string[] {
  return args.split(/\s+/).filter((t) => t.length > 0);
}

function disabledFeatureSwitches(args: string): string[] {
  return tokens(args).filter((t) => t.startsWith("--disable-features="));
}

test("the config declares at least one window — every assertion below runs over them", () => {
  // Positive control: an empty window list would make every per-window check
  // below vacuously green.
  assert.ok(windows().length > 0);
});

test("every window's webview carries each throttling switch (#1141)", () => {
  for (const w of windows()) {
    const args = w.additionalBrowserArgs ?? "";
    const present = new Set(tokens(args));
    for (const s of THROTTLING_SWITCHES) {
      assert.ok(present.has(s), `missing ${s} in additionalBrowserArgs: ${JSON.stringify(args)}`);
    }
  }
});

test("exactly one --disable-features switch, carrying wry's defaults AND the throttling feature", () => {
  for (const w of windows()) {
    const args = w.additionalBrowserArgs ?? "";
    const sw = disabledFeatureSwitches(args);
    assert.equal(sw.length, 1, `expected one --disable-features= switch, got ${JSON.stringify(sw)}`);
    const features = sw[0].slice("--disable-features=".length).split(",");
    for (const f of [...WRY_DEFAULT_DISABLED_FEATURES, ...THROTTLING_FEATURES]) {
      assert.ok(features.includes(f), `--disable-features is missing ${f}: ${sw[0]}`);
    }
  }
});

test("the one-switch rule is a real check: a split list is refused", () => {
  // Discriminator for the rule above, on a synthetic value: the shape a
  // well-meaning append produces (wry's list, then a second switch for ours).
  const split =
    "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection --disable-features=IntensiveWakeUpThrottling";
  assert.equal(disabledFeatureSwitches(split).length, 2);
});
