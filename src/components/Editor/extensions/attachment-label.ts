export function attachmentBasename(path: string): string {
  const clean = path.replace(/[?#].*$/, "");
  return clean.split(/[\\/]/).pop() || clean;
}

export function visibleAttachmentLabel(alt: string, path: string): string | null {
  const label = alt.trim();
  if (!label) return null;

  // Paste/save flows used to store the generated filename as alt text.
  // Treat that as implementation detail, not a user-visible caption.
  return label === attachmentBasename(path) ? null : label;
}
