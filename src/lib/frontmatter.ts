import yaml from "js-yaml";

export interface Frontmatter {
  [key: string]: string | string[] | number | boolean | null;
}

export interface ParsedNote {
  frontmatter: Frontmatter | null;
  body: string;
  rawFrontmatter: string;
}

// Use CORE_SCHEMA to prevent js-yaml from auto-converting dates to Date objects
const YAML_SCHEMA = yaml.CORE_SCHEMA;

export function parseNote(content: string): ParsedNote {
  const match = content.match(/^---\r?\n([\s\S]*?)\r?\n---\r?\n?([\s\S]*)$/);
  if (!match) {
    return { frontmatter: null, body: content, rawFrontmatter: "" };
  }

  try {
    const parsed = yaml.load(match[1], { schema: YAML_SCHEMA }) as Frontmatter;
    return {
      frontmatter: parsed && typeof parsed === "object" ? parsed : null,
      body: match[2],
      rawFrontmatter: match[1],
    };
  } catch {
    return { frontmatter: null, body: content, rawFrontmatter: "" };
  }
}

export function getPropertyType(
  key: string,
  value: unknown
): "date" | "tags" | "text" | "number" | "boolean" | "list" {
  if (Array.isArray(value)) {
    if (key === "tags" || key === "aliases") return "tags";
    return "list";
  }
  if (typeof value === "boolean") return "boolean";
  if (typeof value === "number") return "number";
  if (typeof value === "string") {
    if (/^\d{4}-\d{2}-\d{2}/.test(value)) return "date";
  }
  return "text";
}

export function formatDate(value: string): string {
  try {
    const date = new Date(value);
    return date.toLocaleDateString("en-US", {
      day: "2-digit",
      month: "2-digit",
      year: "numeric",
    });
  } catch {
    return value;
  }
}

/** Serialize frontmatter using js-yaml's dump for correctness */
function serializeFrontmatter(fm: Frontmatter): string {
  return yaml.dump(fm, {
    schema: YAML_SCHEMA,
    lineWidth: -1,       // Don't wrap long lines
    noRefs: true,        // Don't use YAML references
    sortKeys: false,     // Preserve insertion order
    quotingType: '"',    // Use double quotes when quoting
    forceQuotes: false,  // Only quote when necessary
  }).trimEnd();
}

/** Replace frontmatter in content, preserving body. If no frontmatter exists, adds it. */
export function updateFrontmatterInContent(
  content: string,
  newFm: Frontmatter
): string {
  const serialized = serializeFrontmatter(newFm);
  const match = content.match(/^---\r?\n([\s\S]*?)\r?\n---\r?\n?([\s\S]*)$/);
  if (match) {
    return `---\n${serialized}\n---\n${match[2]}`;
  }
  // No existing frontmatter — prepend it
  return `---\n${serialized}\n---\n\n${content}`;
}

/** Set a property in frontmatter and return updated content */
export function setProperty(
  content: string,
  key: string,
  value: Frontmatter[string]
): string {
  const parsed = parseNote(content);
  const fm = { ...(parsed.frontmatter || {}), [key]: value };
  return updateFrontmatterInContent(content, fm);
}

/** Remove a property from frontmatter and return updated content */
export function removeProperty(content: string, key: string): string {
  const parsed = parseNote(content);
  if (!parsed.frontmatter) return content;
  const fm = { ...parsed.frontmatter };
  delete fm[key];
  if (Object.keys(fm).length === 0) {
    return parsed.body;
  }
  return updateFrontmatterInContent(content, fm);
}

/** Remove a tag from an array property and return updated content */
export function removeTagFromProperty(
  content: string,
  key: string,
  tagToRemove: string
): string {
  const parsed = parseNote(content);
  if (!parsed.frontmatter) return content;
  const value = parsed.frontmatter[key];
  if (!Array.isArray(value)) return content;
  const filtered = value.filter((t) => t !== tagToRemove);
  return setProperty(content, key, filtered);
}

/** Add a tag to an array property and return updated content */
export function addTagToProperty(
  content: string,
  key: string,
  newTag: string
): string {
  const parsed = parseNote(content);
  const fm = parsed.frontmatter || {};
  const current = Array.isArray(fm[key]) ? (fm[key] as string[]) : [];
  if (current.includes(newTag)) return content;
  return setProperty(content, key, [...current, newTag]);
}
