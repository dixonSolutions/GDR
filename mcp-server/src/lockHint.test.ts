import { strict as assert } from "node:assert";
import { test } from "node:test";
import { isScreenLockedError, screenLockFields } from "./lockHint.js";

const RAW_MUTTER =
  "start display provider: org.freedesktop.DBus.Error.Failed: Session creation inhibited";

const TRANSLATED =
  "RemoteDesktop.CreateSession: the GNOME session is locked. Mutter refuses " +
  "ScreenCast and RemoteDesktop to unprivileged clients while the lock shield is up";

test("detects the raw Mutter error from an older gdrd", () => {
  assert.equal(isScreenLockedError(RAW_MUTTER), true);
});

test("detects gdrd's translated message", () => {
  assert.equal(isScreenLockedError(TRANSLATED), true);
});

test("leaves unrelated errors alone", () => {
  assert.equal(isScreenLockedError("auth failed: token revoked"), false);
  assert.deepEqual(screenLockFields("ECONNREFUSED 127.0.0.1:7337"), {});
});

test("marks a locked screen as not retryable and names the remedy", () => {
  const fields = screenLockFields(RAW_MUTTER) as Record<string, unknown>;
  assert.equal(fields.screen_locked, true);
  assert.equal(fields.retryable, false);
  assert.match(String(fields.remedy), /loginctl unlock-session/);
});
