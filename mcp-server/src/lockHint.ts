/**
 * Recognising "the target's screen is locked" in an error coming back from
 * gdrd.
 *
 * Mutter refuses ScreenCast and RemoteDesktop to unprivileged clients while
 * the lock shield is up, and the D-Bus error it returns ("Session creation
 * inhibited") names neither the lock nor a remedy. gdrd now translates that
 * into a sentence that does; this module lets every MCP tool spot either
 * form and tell the agent to stop retrying and ask for an unlock, which is
 * the only thing that fixes it.
 */

/** Matches gdrd's translated message and the raw Mutter error behind it. */
export const LOCKED_SESSION_RE = /the GNOME session is locked|Session creation inhibited/i;

export function isScreenLockedError(message: string): boolean {
  return LOCKED_SESSION_RE.test(message);
}

export const SCREEN_LOCKED_REMEDY =
  "The target's screen is locked; Mutter blocks screen capture and input " +
  "until it is unlocked. Ask the user to unlock it, or run " +
  "`loginctl unlock-session <id>` on the target. No gdrd restart or config " +
  "change is needed — retry the same tool once it is unlocked.";

/** Extra fields to merge into an MCP error payload. Empty when unrelated. */
export function screenLockFields(message: string) {
  if (!isScreenLockedError(message)) return {};
  return {
    screen_locked: true,
    // Retrying is pointless until a human unlocks; say so, so an agent does
    // not burn a retry budget on a call that cannot succeed.
    retryable: false,
    remedy: SCREEN_LOCKED_REMEDY,
  };
}
