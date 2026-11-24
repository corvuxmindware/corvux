/// Roost facts the pairing UI needs, kept as pure functions so both pairing
/// surfaces (the code/QR panel and the Nearby list) share ONE copy and the
/// rules are unit-testable without mounting a component.

/// Mirror the Rust-side `roost_folder_name` helper exactly: SAF
/// `content://` URIs URL-decode then take the last segment; regular paths
/// just take the last separator-split segment. The two MUST agree — this is
/// the name the sync handshake compares, so a mismatch here would grey out a
/// pairing the handshake would accept (or promise one it will refuse).
export function roostFolderName(path: string | null | undefined): string {
  if (!path) return "";
  if (path.startsWith("content://")) {
    const afterScheme = path.slice("content://".length);
    let decoded: string;
    try {
      decoded = decodeURIComponent(afterScheme);
    } catch {
      decoded = afterScheme;
    }
    const segs = decoded.split("/").filter(Boolean);
    return segs[segs.length - 1] ?? "";
  }
  return path.split(/[\\/]/).filter(Boolean).pop() ?? "";
}

export interface MergeConfirmation {
  roost: string;
  notes: number;
}

/// Pairing does NOT check that the two devices hold the same roost — only
/// that the folder names match — so a first pair union-merges whatever both
/// roosts contain, permanently. When THIS device already has notes, the user
/// must confirm that merge before we dial. An empty roost is a plain "join"
/// (it just receives the other device's notes), so no prompt.
export function mergeConfirmationFor(
  roostPath: string | null | undefined,
  noteCount: number,
): MergeConfirmation | null {
  if (noteCount <= 0) return null;
  return { roost: roostFolderName(roostPath), notes: noteCount };
}
